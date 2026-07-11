#!/usr/bin/env python3
"""Verify the exact publishable Cargo source package surface."""

from __future__ import annotations

import argparse
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile
import tomllib
from typing import Iterable

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


class PackageError(RuntimeError):
    pass


def validate_relative_path(path: str) -> None:
    parsed = PurePosixPath(path)
    if (
        not path
        or "\\" in path
        or "\0" in path
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
    members: Iterable[tarfile.TarInfo], root_prefix: str
) -> set[str]:
    validate_relative_path(root_prefix)
    entries: set[str] = set()
    seen_members: set[str] = set()
    for member in members:
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
            entry = PurePosixPath(*path.parts[1:]).as_posix()
            validate_relative_path(entry)
            entries.add(entry)
    return entries


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


def package_entries(cargo: str, allow_dirty: bool) -> set[str]:
    package = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    root_prefix = f"{package['name']}-{package['version']}"
    archive = ROOT / "target" / "package" / f"{root_prefix}.crate"
    archive.unlink(missing_ok=True)
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
    try:
        with tarfile.open(archive, mode="r:gz") as package_archive:
            return validate_archive_members(package_archive, root_prefix)
    except (OSError, tarfile.TarError) as error:
        raise PackageError(f"could not inspect Cargo package archive: {error}") from error


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
