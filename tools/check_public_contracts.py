#!/usr/bin/env python3
"""Scan packaged public text and validate curated documented CLI contracts."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import gzip
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import stat
import subprocess
import sys
import tarfile
from typing import BinaryIO, Iterable, Mapping
import unicodedata


MAX_DIAGNOSTICS = 32
MAX_DIAGNOSTIC_CHARS = 2048
SHELL_FENCES = {"sh", "bash", "shell", "console"}
COMMAND_ALLOWLIST = {"herdr-tether", "cargo"}
TOKEN_PATTERNS = (
    re.compile(r"\b(?:api[_-]?key|api[_-]?token|access[_-]?token|secret|password)\s*[:=]", re.I),
    re.compile(r"\b(?:gh[opsu]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b"),
    re.compile(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
    re.compile(r"\b(?:sk|pk)_(?:live|test)_[A-Za-z0-9]{16,}\b"),
)
TEXT_PATTERNS = (
    ("private-path", re.compile(r"(?<![A-Za-z0-9_])(?:/home/[^/\s]+|/Users/[^/\s]+|[A-Za-z]:[\\/]Users[\\/][^\\/\s]+)")),
    ("private-host", re.compile(r"\b[A-Za-z0-9._-]+@(?:localhost|[A-Za-z0-9-]+\.(?:local|lan|internal))\b", re.I)),
    ("private-key", re.compile(r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----")),
    ("orchestration-residue", re.compile(r"\b(?:mission ledger|internal ledger|agent transcript|internal mission|conversation transcript)\b", re.I)),
    ("unsafe-release-wording", re.compile(r"\b(?:curl|wget)\b[^\n]{0,240}\|\s*(?:ba)?sh\b|\bsudo\s+(?:cargo\s+install|herdr\s+plugin\s+install)\b", re.I)),
)
SHELL_OPERATOR_RE = re.compile(r"(?:\|\||&&|[|&;<>`]|\$\(|\$\{|\n)")
ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
SHA_RE = re.compile(r"^[0-9a-fA-F]{40}$")


class ContractError(RuntimeError):
    """A packaged public contract is unsafe or cannot be verified."""


@dataclass(frozen=True)
class ArchiveLimits:
    max_members: int = 128
    max_member_bytes: int = 2 * 1024 * 1024
    max_total_bytes: int = 8 * 1024 * 1024
    max_archive_bytes: int = 2 * 1024 * 1024
    max_decompressed_bytes: int = 16 * 1024 * 1024


DEFAULT_ARCHIVE_LIMITS = ArchiveLimits()


class _BoundedReader:
    def __init__(self, source: BinaryIO, limit: int) -> None:
        self.source = source
        self.remaining = limit

    def read(self, size: int = -1) -> bytes:
        requested = self.remaining + 1 if size < 0 else min(size, self.remaining + 1)
        data = self.source.read(requested)
        if len(data) > self.remaining:
            raise ContractError("package archive decompression exceeds safety limit")
        self.remaining -= len(data)
        return data


@dataclass(frozen=True)
class CliExample:
    surface: str
    line: int
    argv: tuple[str, ...]
    kind: str


def _safe_member_name(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if any(unicodedata.category(character).startswith("C") for character in name):
        raise ContractError("package archive contains an unsafe member path")
    if (
        not name
        or "\\" in name
        or "\0" in name
        or path.is_absolute()
        or any(part in ("", ".", "..") for part in path.parts)
        or path.as_posix() != name.rstrip("/")
    ):
        raise ContractError("package archive contains an unsafe member path")
    return path


def _decode_public_text(contents: bytes) -> str:
    if b"\0" in contents:
        raise ContractError("malformed-public-text")
    try:
        return contents.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError("malformed-public-text") from error


def _scan_text(text: str) -> set[str]:
    categories: set[str] = set()
    if any(pattern.search(text) for pattern in TOKEN_PATTERNS):
        categories.add("credential-token")
    for category, pattern in TEXT_PATTERNS:
        if pattern.search(text):
            categories.add(category)
    if any(
        character not in "\n\r\t" and unicodedata.category(character) in {"Cc", "Cf"}
        for character in text
    ):
        categories.add("unicode-format")
    return categories


def _format_violations(violations: list[tuple[str, str]], maximum: int) -> str:
    shown = sorted(set(violations))[:maximum]
    parts = [f"{path}: {category}" for path, category in shown]
    omitted = len(set(violations)) - len(shown)
    if omitted:
        parts.append(f"{omitted} additional violation(s) omitted")
    message = "public contract violations: " + "; ".join(parts)
    return message[:MAX_DIAGNOSTIC_CHARS]


BINARY_ASSET_SUFFIXES = {".bin", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".webp", ".woff", ".woff2"}


def _is_public_text_surface(path: str) -> bool:
    parsed = PurePosixPath(path)
    if parsed.parts[0] in {"docs", "integrations"}:
        return True
    if parsed.parts[0] == "assets":
        return parsed.suffix.lower() not in BINARY_ASSET_SUFFIXES
    return len(parsed.parts) == 1 and (
        parsed.suffix.lower() in {".md", ".txt", ".toml", ".json"}
        or parsed.name in {"LICENSE", "Cargo.lock", "Cargo.toml.orig"}
    )


def inspect_archive(
    archive_path: Path,
    *,
    max_diagnostics: int = MAX_DIAGNOSTICS,
    limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS,
) -> list[CliExample]:
    """Inspect bounded regular files in a Cargo package; return curated CLI examples."""
    if max_diagnostics < 1:
        raise ValueError("max_diagnostics must be positive")
    violations: list[tuple[str, str]] = []
    examples: list[CliExample] = []
    seen: set[str] = set()
    package_root: str | None = None
    total_bytes = 0
    try:
        nofollow = getattr(os, "O_NOFOLLOW", None)
        if nofollow is None:
            raise ContractError("package archive no-follow open is unavailable")
        flags = os.O_RDONLY | nofollow | getattr(os, "O_CLOEXEC", 0)
        descriptor = os.open(archive_path, flags)
        try:
            raw_archive = os.fdopen(descriptor, "rb")
        except Exception:
            os.close(descriptor)
            raise
        with raw_archive:
            opened = os.fstat(raw_archive.fileno())
            if not stat.S_ISREG(opened.st_mode):
                raise ContractError("package archive is not a regular file")
            if opened.st_size > limits.max_archive_bytes:
                raise ContractError("package archive exceeds compressed-byte safety limit")
            with gzip.GzipFile(fileobj=raw_archive, mode="rb") as decompressed:
                bounded = _BoundedReader(decompressed, limits.max_decompressed_bytes)
                with tarfile.open(fileobj=bounded, mode="r|") as archive:
                    for count, member in enumerate(archive, start=1):
                        if count > limits.max_members:
                            raise ContractError("package archive exceeds member-count safety limit")
                        path = _safe_member_name(member.name.rstrip("/") if member.isdir() else member.name)
                        if package_root is None:
                            package_root = path.parts[0]
                        if not path.parts or path.parts[0] != package_root:
                            raise ContractError("package archive member escapes its package root")
                        if path.as_posix() in seen:
                            raise ContractError("package archive repeats a member")
                        seen.add(path.as_posix())
                        if len(path.parts) == 1:
                            if not member.isdir():
                                raise ContractError("package archive root is not a directory")
                            continue
                        if not member.isfile() and not member.isdir():
                            raise ContractError("package archive contains a non-regular public member")
                        if member.isdir():
                            continue
                        if member.size < 0 or member.size > limits.max_member_bytes:
                            raise ContractError("package archive member exceeds per-member safety limit")
                        total_bytes += member.size
                        if total_bytes > limits.max_total_bytes:
                            raise ContractError("package archive exceeds total-byte safety limit")
                        relative = PurePosixPath(*path.parts[1:]).as_posix()
                        if not _is_public_text_surface(relative):
                            continue
                        handle = archive.extractfile(member)
                        if handle is None:
                            raise ContractError("package archive member could not be inspected")
                        contents = handle.read(member.size + 1)
                        if len(contents) != member.size:
                            raise ContractError("package archive member is truncated or oversized")
                        try:
                            text = _decode_public_text(contents)
                        except ContractError:
                            violations.append((relative, "malformed-public-text"))
                            continue
                        violations.extend((relative, category) for category in _scan_text(text))
                        if relative.lower().endswith((".md", ".markdown")):
                            try:
                                examples.extend(extract_cli_examples(text, relative))
                            except ContractError:
                                violations.append((relative, "unsafe-cli-example"))
    except ContractError:
        raise
    except (OSError, EOFError, tarfile.TarError) as error:
        raise ContractError("package archive could not be inspected") from error
    if violations:
        raise ContractError(_format_violations(violations, max_diagnostics))
    return sorted(examples, key=lambda example: (example.surface, example.line, example.argv))


def _logical_commands(lines: list[tuple[int, str]]) -> Iterable[tuple[int, str]]:
    pending = ""
    start = 0
    for line_number, raw_line in lines:
        line = raw_line.strip()
        if line.startswith("$ "):
            line = line[2:].lstrip()
        if not line or line.startswith("#"):
            continue
        if not pending:
            start = line_number
        if line.endswith("\\"):
            pending += line[:-1].rstrip() + " "
            continue
        yield start, pending + line
        pending = ""
    if pending:
        raise ContractError("documented command has an incomplete continuation")


def _example_kind(argv: list[str]) -> str:
    if argv[0] == "herdr-tether":
        return "cli"
    if argv[:2] != ["cargo", "install"]:
        raise ContractError("documented command is outside the curated allowlist")
    selectors = [("--tag", "stable"), ("--branch", "main"), ("--rev", "exact-sha")]
    found = [(option, kind) for option, kind in selectors if option in argv]
    if len(found) != 1:
        raise ContractError("cargo install example must use exactly one release selector")
    option, kind = found[0]
    try:
        value = argv[argv.index(option) + 1]
    except IndexError as error:
        raise ContractError("cargo install example has an incomplete release selector") from error
    if kind == "stable" and not re.fullmatch(r"v\d+\.\d+\.\d+", value):
        raise ContractError("cargo install tag example is not a stable version")
    if kind == "main" and value != "main":
        raise ContractError("cargo install branch example is not main")
    if kind == "exact-sha" and value != "$TETHER_REV" and not SHA_RE.fullmatch(value):
        raise ContractError("cargo install revision example is not an exact SHA placeholder")
    return kind


def _parse_curated_command(command: str, surface: str, line: int) -> CliExample | None:
    leading = command.lstrip().split(maxsplit=1)
    if not leading:
        return None
    first = leading[0]
    if ASSIGNMENT_RE.match(first):
        if re.search(r"(?:^|\s)(?:herdr-tether|cargo\s+install)(?:\s|$)", command):
            raise ContractError(f"{surface}:{line}: environment assignment before command is forbidden")
        return None
    if first not in COMMAND_ALLOWLIST:
        return None
    if first == "cargo" and not re.match(r"^cargo\s+install(?:\s|$)", command):
        return None
    if SHELL_OPERATOR_RE.search(command):
        raise ContractError(f"{surface}:{line}: documented command uses forbidden shell syntax")
    try:
        argv = shlex.split(command, posix=True)
    except ValueError as error:
        raise ContractError(f"{surface}:{line}: documented command cannot be tokenized") from error
    return CliExample(surface, line, tuple(argv), _example_kind(argv))


def extract_cli_examples(markdown: str, surface: str) -> list[CliExample]:
    """Extract allowlisted commands from shell fences without invoking a shell."""
    examples: list[CliExample] = []
    fence: str | None = None
    body: list[tuple[int, str]] = []
    for line_number, raw_line in enumerate(markdown.splitlines(), 1):
        stripped = raw_line.strip()
        if fence is None:
            match = re.fullmatch(r"```([A-Za-z0-9_-]*)", stripped)
            if match and match.group(1).lower() in SHELL_FENCES:
                fence = match.group(1).lower()
                body = []
            continue
        if stripped == "```":
            for command_line, command in _logical_commands(body):
                example = _parse_curated_command(command, surface, command_line)
                if example is not None:
                    examples.append(example)
            fence = None
            body = []
        else:
            body.append((line_number, raw_line))
    return examples


def _help_argv(example: CliExample, binary: Path) -> list[str] | None:
    if example.argv[0] != "herdr-tether":
        return None
    words = list(example.argv[1:])
    root = words[0] if words else ""
    two_level = {"host", "session", "orchestration"}
    command = [str(binary)]
    if root:
        command.append(root)
    if root in two_level and len(words) > 1 and not words[1].startswith("-"):
        command.append(words[1])
    command.append("--help")
    return command


def validate_release_reference_kinds(examples: Iterable[CliExample]) -> None:
    kinds = {example.kind for example in examples if example.argv[:2] == ("cargo", "install")}
    if kinds and kinds != {"stable", "main", "exact-sha"}:
        raise ContractError("documented cargo installs must keep stable, main, and exact-SHA examples distinct")


def validate_cli_examples(
    examples: Iterable[CliExample],
    binary: Path,
    *,
    environment: Mapping[str, str] | None = None,
) -> None:
    """Run only deterministic help projections against the supplied built binary."""
    ordered = sorted(examples, key=lambda example: (example.surface, example.line, example.argv))
    validate_release_reference_kinds(ordered)
    if not binary.is_file():
        raise ContractError("supplied CLI binary is unavailable")
    child_environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"}
    if environment:
        child_environment.update(environment)
    for example in ordered:
        command = _help_argv(example, binary)
        if command is None:
            continue
        try:
            result = subprocess.run(
                command,
                check=False,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=10,
                env=child_environment,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise ContractError(f"{example.surface}:{example.line}: CLI help contract could not run") from error
        if result.returncode != 0:
            raise ContractError(f"{example.surface}:{example.line}: CLI help contract failed")


def parse_args(argv: Iterable[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", required=True, type=Path, help="built Cargo .crate archive")
    parser.add_argument("--binary", required=True, type=Path, help="built herdr-tether binary")
    return parser.parse_args(argv)


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        examples = inspect_archive(args.archive)
        validate_cli_examples(examples, args.binary)
    except ContractError as error:
        print(f"public contract error: {error}", file=sys.stderr)
        return 1
    print(f"public contracts verified: {len(examples)} curated example(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
