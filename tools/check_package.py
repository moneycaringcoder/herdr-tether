#!/usr/bin/env python3
"""Verify the exact publishable Cargo source package surface."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import gzip
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from typing import BinaryIO, Iterable
import unicodedata

ROOT = Path(__file__).resolve().parents[1]
REQUIRED_FILES = {
    "Cargo.lock",
    "Cargo.toml",
    "CHANGELOG.md",
    "CONTRIBUTING.md",
    "LICENSE",
    "README.md",
    "SECURITY.md",
    "herdr-plugin.toml",
}
REQUIRED_PREFIXES = ("docs/", "src/")
GENERATED_FILES = {".cargo_vcs_info.json", "Cargo.toml.orig"}
ALLOWED_PREFIXES = ("assets/", "docs/", "src/", "tests/")
CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
CHECKSUM_RE = re.compile(r"^[0-9a-f]{64}$")
EXPECTED_PACKAGE_FILES = frozenset(
    {
        ".cargo_vcs_info.json",
        "CHANGELOG.md",
        "CONTRIBUTING.md",
        "Cargo.lock",
        "Cargo.toml",
        "Cargo.toml.orig",
        "LICENSE",
        "README.md",
        "SECURITY.md",
        "assets/README.md",
        "assets/social-preview.svg",
        "assets/tether-mark.svg",
        "assets/tether-wordmark.svg",
        "docs/architecture.md",
        "docs/configuration.md",
        "docs/lifecycle.md",
        "docs/quickstart.md",
        "docs/troubleshooting.md",
        "herdr-plugin.toml",
        "src/agent_view.rs",
        "src/backend.rs",
        "src/cli.rs",
        "src/config.rs",
        "src/discovery.rs",
        "src/herdr.rs",
        "src/herdr_socket.rs",
        "src/lib.rs",
        "src/lifecycle.rs",
        "src/main.rs",
        "src/mission_control.rs",
        "src/model.rs",
        "src/observer.rs",
        "src/observer_manager.rs",
        "src/orchestration.rs",
        "src/paths.rs",
        "src/quote.rs",
        "src/snapshot.rs",
        "src/sshcfg.rs",
        "src/state.rs",
        "src/status.rs",
        "src/storage.rs",
        "src/tmux.rs",
        "src/tui.rs",
        "tests/backend_lifecycle.rs",
        "tests/cli.rs",
        "tests/config_state.rs",
        "tests/discovery.rs",
        "tests/herdr_picker.rs",
        "tests/hosts.rs",
        "tests/observer.rs",
        "tests/observer_manager.rs",
        "tests/plugin_manifest.rs",
        "tests/snapshot.rs",
        "tests/status.rs",
    }
)


@dataclass(frozen=True)
class ArchiveLimits:
    max_members: int = 128
    max_member_bytes: int = 2 * 1024 * 1024
    max_total_bytes: int = 8 * 1024 * 1024
    max_archive_bytes: int = 2 * 1024 * 1024

    @property
    def max_decompressed_stream_bytes(self) -> int:
        # A tar has 512-byte records plus optional format metadata. Keep that
        # overhead bounded independently of the declared regular-file sizes.
        return self.max_total_bytes + (self.max_members + 4) * 64 * 1024


DEFAULT_ARCHIVE_LIMITS = ArchiveLimits()


class PackageError(RuntimeError):
    pass


def validate_relative_path(path: str) -> None:
    parsed = PurePosixPath(path)
    if (
        not path
        or "\\" in path
        or "\0" in path
        or any(ord(character) < 32 or ord(character) == 127 for character in path)
        or unicodedata.normalize("NFC", path) != path
        or parsed.is_absolute()
        or any(part in ("", ".", "..") for part in parsed.parts)
        or parsed.as_posix() != path
    ):
        raise PackageError(f"package contains unsafe path {path!r}")


def is_allowed_entry(entry: str) -> bool:
    return (
        entry in REQUIRED_FILES
        or entry in GENERATED_FILES
        or any(entry.startswith(prefix) for prefix in ALLOWED_PREFIXES)
    )


def validate_entries(entries: set[str], required_assets: set[str]) -> None:
    for entry in entries:
        validate_relative_path(entry)
    missing = sorted((REQUIRED_FILES | required_assets) - entries)
    if missing:
        raise PackageError(f"package omits required public files: {', '.join(missing)}")
    if entries != EXPECTED_PACKAGE_FILES:
        missing_expected = sorted(EXPECTED_PACKAGE_FILES - entries)
        added = sorted(entries - EXPECTED_PACKAGE_FILES)
        details = []
        if missing_expected:
            details.append(f"missing: {', '.join(missing_expected)}")
        if added:
            details.append(f"unexpected: {', '.join(added)}")
        raise PackageError(
            f"package surface differs from the explicit {len(EXPECTED_PACKAGE_FILES)}-file contract ("
            + "; ".join(details)
            + ")"
        )
    for prefix in REQUIRED_PREFIXES:
        if not any(entry.startswith(prefix) for entry in entries):
            raise PackageError(f"package contains no {prefix} files")
    unexpected = sorted(entry for entry in entries if not is_allowed_entry(entry))
    if unexpected:
        raise PackageError(
            f"package exposes files outside the public allowlist: {', '.join(unexpected)}"
        )


def validate_lock_packages(packages: list[dict[str, object]]) -> None:
    local = [package for package in packages if "source" not in package]
    if len(local) != 1 or local[0].get("name") != "herdr-tether":
        names = ", ".join(str(package.get("name", "<unnamed>")) for package in local)
        raise PackageError(f"unexpected local/path packages in Cargo.lock: {names}")
    for package in packages:
        source = package.get("source")
        if source is None:
            continue
        name = str(package.get("name", "<unnamed>"))
        if source != CRATES_IO_SOURCE:
            raise PackageError(f"locked dependency {name} uses unapproved source {source!r}")
        checksum = package.get("checksum")
        if not isinstance(checksum, str) or not CHECKSUM_RE.fullmatch(checksum):
            raise PackageError(f"locked registry dependency {name} has no SHA-256 checksum")


def validate_source_paths(root: Path, entries: set[str]) -> None:
    for entry in entries:
        validate_relative_path(entry)
        if entry in GENERATED_FILES:
            continue
        candidate = root / entry
        current = root
        for part in PurePosixPath(entry).parts:
            current /= part
            if current.is_symlink():
                raise PackageError(f"package source path is a symlink: {entry}")
        if not candidate.is_file():
            raise PackageError(f"package source is not a regular file: {entry}")


def validate_archive_members(
    members: Iterable[tarfile.TarInfo],
    root_prefix: str,
    limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS,
) -> set[str]:
    validate_relative_path(root_prefix)
    entries: set[str] = set()
    seen_members: set[str] = set()
    total_bytes = 0
    for count, member in enumerate(members, start=1):
        if count > limits.max_members:
            raise PackageError(
                f"package archive exceeds {limits.max_members} members"
            )
        member_name = (
            member.name[:-1]
            if member.isdir() and member.name.endswith("/")
            else member.name
        )
        validate_relative_path(member_name)
        if member_name in seen_members:
            raise PackageError(f"package archive repeats member {member.name!r}")
        seen_members.add(member_name)
        path = PurePosixPath(member_name)
        if not path.parts or path.parts[0] != root_prefix:
            raise PackageError(f"package archive member escapes package root: {member.name!r}")
        if len(path.parts) == 1:
            if not member.isdir():
                raise PackageError("package archive root is not a directory")
            continue
        if not member.isfile() and not member.isdir():
            raise PackageError(
                f"package archive contains link or special member {member.name!r}"
            )
        if member.isfile():
            if member.size < 0 or member.size > limits.max_member_bytes:
                raise PackageError(
                    f"package archive member {member.name!r} exceeds "
                    f"{limits.max_member_bytes} bytes"
                )
            total_bytes += member.size
            if total_bytes > limits.max_total_bytes:
                raise PackageError(
                    f"package archive exceeds {limits.max_total_bytes} total bytes"
                )
            entry = PurePosixPath(*path.parts[1:]).as_posix()
            validate_relative_path(entry)
            entries.add(entry)
    return entries


class _BoundedReader:
    def __init__(self, source: BinaryIO, limit: int) -> None:
        self.source = source
        self.remaining = limit

    def read(self, size: int = -1) -> bytes:
        requested = self.remaining + 1 if size < 0 else min(size, self.remaining + 1)
        data = self.source.read(requested)
        if len(data) > self.remaining:
            raise PackageError("package archive decompression exceeds safety limit")
        self.remaining -= len(data)
        return data


def _read_validated_members(
    package_archive: tarfile.TarFile,
    package_root: Path | None = None,
) -> Iterable[tarfile.TarInfo]:
    for member in package_archive:
        # The consumer validates the header before asking this generator to
        # consume or materialize its payload.
        yield member
        if not member.isfile():
            continue
        extracted = package_archive.extractfile(member)
        if extracted is None:
            raise PackageError(f"could not read package member {member.name!r}")
        remaining = member.size
        output = None
        relative = None
        try:
            if package_root is not None:
                relative = PurePosixPath(member.name).relative_to(package_root.name)
                target = package_root.joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                output = target.open("xb")
            while remaining:
                chunk = extracted.read(min(64 * 1024, remaining))
                if not chunk:
                    raise PackageError(f"package member {member.name!r} is truncated")
                if output is not None:
                    output.write(chunk)
                remaining -= len(chunk)
        except FileExistsError as error:
            assert relative is not None
            raise PackageError(
                f"package extraction repeats destination {relative.as_posix()!r}"
            ) from error
        finally:
            if output is not None:
                output.close()


def _consume_archive(
    archive: Path,
    root_prefix: str,
    limits: ArchiveLimits,
    destination: Path | None = None,
) -> set[str]:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = -1
    try:
        descriptor = os.open(archive, flags)
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise PackageError(f"Cargo package archive is not a regular file: {archive}")
        if metadata.st_size > limits.max_archive_bytes:
            raise PackageError(
                f"package archive exceeds {limits.max_archive_bytes} compressed bytes"
            )
        package_root = destination / root_prefix if destination is not None else None
        with os.fdopen(descriptor, "rb") as raw_archive:
            descriptor = -1
            with gzip.GzipFile(fileobj=raw_archive, mode="rb") as decompressed:
                bounded = _BoundedReader(
                    decompressed, limits.max_decompressed_stream_bytes
                )
                with tarfile.open(fileobj=bounded, mode="r|") as package_archive:
                    return validate_archive_members(
                        _read_validated_members(package_archive, package_root),
                        root_prefix,
                        limits,
                    )
    except PackageError:
        raise
    except (OSError, EOFError, tarfile.TarError) as error:
        raise PackageError(f"could not inspect Cargo package archive: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def inspect_archive(
    archive: Path,
    root_prefix: str,
    limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS,
) -> set[str]:
    return _consume_archive(archive, root_prefix, limits)


def extract_validated_archive(
    archive: Path, root_prefix: str, destination: Path
) -> Path:
    """Validate and materialize regular files from one securely opened inode."""
    _consume_archive(archive, root_prefix, DEFAULT_ARCHIVE_LIMITS, destination)
    return destination / root_prefix


def _run_runtime_command(
    command: list[str],
    environment: dict[str, str],
    *,
    expected_code: int = 0,
    timeout: int = 30,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            command,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PackageError(f"installed runtime command failed to execute: {error}") from error
    if result.returncode != expected_code:
        rendered = " ".join(command[1:])
        raise PackageError(
            f"installed runtime `{rendered}` exited {result.returncode}, "
            f"expected {expected_code}: {result.stderr[:1000]}"
        )
    return result


def _write_probe(path: Path, output: str) -> None:
    path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{output}'\n", encoding="utf-8")
    path.chmod(0o700)


def isolated_build_environment(cargo_home: Path, home: Path) -> dict[str, str]:
    environment = os.environ.copy()
    environment.pop("HERDR_SOCKET_PATH", None)
    if not environment.get("RUSTUP_HOME"):
        original_home = Path(environment.get("HOME", str(Path.home())))
        default_rustup_home = original_home / ".rustup"
        if default_rustup_home.is_dir():
            environment["RUSTUP_HOME"] = str(default_rustup_home)
    environment.update(
        {
            "CARGO_HOME": str(cargo_home),
            "CARGO_NET_OFFLINE": "true",
            "HOME": str(home),
        }
    )
    return environment


def verify_installed_runtime(cargo: str, archive: Path, root_prefix: str) -> None:
    cargo_path = shutil.which(cargo)
    if cargo_path is None:
        raise PackageError(f"Cargo executable is unavailable: {cargo}")
    with tempfile.TemporaryDirectory(prefix="tether-package-runtime-") as directory:
        sandbox = Path(directory)
        source = extract_validated_archive(archive, root_prefix, sandbox / "source")
        cargo_home = sandbox / "cargo-home"
        install_root = sandbox / "install"
        target_dir = sandbox / "build"
        home = sandbox / "home"
        xdg_config = sandbox / "config"
        xdg_state = sandbox / "state"
        probes = sandbox / "probes"
        for path in (cargo_home, install_root, home, xdg_config, xdg_state, probes):
            path.mkdir(parents=True)

        existing_cargo_home = Path(
            os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))
        )
        registry = existing_cargo_home / "registry"
        if registry.is_dir():
            (cargo_home / "registry").symlink_to(registry, target_is_directory=True)

        build_environment = isolated_build_environment(cargo_home, home)
        install = _run_runtime_command(
            [
                cargo_path,
                "install",
                "--offline",
                "--locked",
                "--no-track",
                "--root",
                str(install_root),
                "--target-dir",
                str(target_dir),
                "--path",
                str(source),
            ],
            build_environment,
            timeout=180,
        )
        binary = install_root / "bin" / "herdr-tether"
        if not binary.is_file():
            raise PackageError("cargo install did not produce herdr-tether")

        for name, output in (
            ("cargo", "cargo 1.88.0"),
            ("herdr", "herdr 0.1.0"),
            ("ssh", "OpenSSH_9.0"),
            ("tmux", "tmux 3.4"),
        ):
            _write_probe(probes / name, output)
        runtime_environment = build_environment | {
            "HERDR_BIN_PATH": str(probes / "herdr"),
            "HERDR_PANE_ID": "package-smoke-pane",
            "HERDR_WORKSPACE_ID": "package-smoke-workspace",
            "PATH": str(probes),
            "XDG_CONFIG_HOME": str(xdg_config),
            "XDG_STATE_HOME": str(xdg_state),
        }
        repository_paths = (str(ROOT), str(ROOT / "target"))

        help_result = _run_runtime_command(
            [str(binary), "setup", "--help"], runtime_environment
        )
        if not home_empty(home) or any(xdg_config.iterdir()) or any(xdg_state.iterdir()):
            raise PackageError("setup --help mutated isolated user paths")

        setup_result = _run_runtime_command(
            [str(binary), "setup", "--yes"], runtime_environment
        )
        expected_files = {
            xdg_config / "herdr-tether" / "config.toml",
            xdg_config / "herdr-tether" / ".config.toml.lock",
            xdg_state / "herdr-tether" / "state.json",
            xdg_state / "herdr-tether" / ".state.json.lock",
        }
        actual_files = {
            path for root in (xdg_config, xdg_state) for path in root.rglob("*") if path.is_file()
        }
        if actual_files != expected_files:
            raise PackageError("installed setup wrote an unexpected isolated file surface")
        if not home_empty(home):
            raise PackageError("installed runtime mutated isolated HOME")

        doctor_result = _run_runtime_command(
            [str(binary), "doctor", "--json"], runtime_environment
        )
        try:
            doctor = json.loads(doctor_result.stdout)
        except json.JSONDecodeError as error:
            raise PackageError("installed doctor did not emit valid JSON") from error
        if (
            doctor.get("schema_version") != 1
            or doctor.get("completion") != "complete"
            or doctor.get("failure_count") != 0
            or not isinstance(doctor.get("checks"), list)
        ):
            raise PackageError("installed doctor JSON violates the stable success contract")

        snapshot_result = _run_runtime_command(
            [str(binary), "snapshot"], runtime_environment
        )
        try:
            snapshot = json.loads(snapshot_result.stdout)
        except json.JSONDecodeError as error:
            raise PackageError("installed snapshot did not emit valid JSON") from error
        if snapshot.get("schema_version") != 1:
            raise PackageError("installed snapshot JSON has an unexpected schema version")

        results = (
            install,
            help_result,
            setup_result,
            doctor_result,
            snapshot_result,
            _run_runtime_command([str(binary), "--help"], runtime_environment),
            _run_runtime_command([str(binary), "--version"], runtime_environment),
        )
        combined = "\n".join(result.stdout + result.stderr for result in results)
        leaked = next((path for path in repository_paths if path in combined), None)
        if leaked is not None:
            raise PackageError("installed runtime output leaked a source or target path")


def home_empty(home: Path) -> bool:
    return next(home.iterdir(), None) is None


def public_assets(root: Path) -> set[str]:
    assets = root / "assets"
    if not assets.is_dir():
        return set()
    result: set[str] = set()
    for path in assets.rglob("*"):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            raise PackageError(f"public asset path is a symlink: {relative}")
        if path.is_file():
            result.add(relative)
    return result


def populate_dependency_cache(cargo: str) -> None:
    """Fetch locked sources before the isolated offline install proof."""
    try:
        result = subprocess.run(
            [cargo, "fetch", "--locked"],
            cwd=ROOT,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=180,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PackageError(f"could not fetch locked Cargo dependencies: {error}") from error
    if result.returncode != 0:
        raise PackageError(
            f"cargo fetch failed ({result.returncode}):\n{result.stderr[:1000]}"
        )


def package_entries(cargo: str, allow_dirty: bool) -> set[str]:
    package = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    root_prefix = f"{package['name']}-{package['version']}"
    archive = ROOT / "target" / "package" / f"{root_prefix}.crate"
    archive.unlink(missing_ok=True)
    populate_dependency_cache(cargo)
    command = [cargo, "package", "--locked", "--no-verify"]
    if allow_dirty:
        command.append("--allow-dirty")
    try:
        result = subprocess.run(
            command,
            cwd=ROOT,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=180,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PackageError(f"could not build Cargo package: {error}") from error
    if result.returncode != 0:
        raise PackageError(
            f"cargo package failed ({result.returncode}):\n{result.stderr}"
        )
    if not archive.is_file():
        raise PackageError(f"cargo package did not create expected archive {archive}")
    entries = inspect_archive(archive, root_prefix)
    validate_entries(entries, public_assets(ROOT))
    verify_installed_runtime(cargo, archive, root_prefix)
    return entries


def parse_args(argv: Iterable[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", default="cargo", help="Cargo executable")
    parser.add_argument("--allow-dirty", action="store_true")
    return parser.parse_args(argv)


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        entries = package_entries(args.cargo, args.allow_dirty)
        validate_entries(entries, public_assets(ROOT))
        validate_source_paths(ROOT, entries)
        with (ROOT / "Cargo.lock").open("rb") as lock_file:
            lock_data = tomllib.load(lock_file)
        validate_lock_packages(lock_data.get("package", []))
    except PackageError as error:
        print(f"package contents error: {error}", file=sys.stderr)
        return 1
    print(f"package contents verified: {len(entries)} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
