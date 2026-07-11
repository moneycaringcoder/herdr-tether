#!/usr/bin/env python3
"""Verify the exact publishable Cargo source package surface."""

from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys
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
FORBIDDEN_PREFIXES = (".git", ".github/", ".omp/", "tools/")


class PackageError(RuntimeError):
    pass


def validate_entries(entries: set[str], required_assets: set[str]) -> None:
    missing = sorted((REQUIRED_FILES | required_assets) - entries)
    if missing:
        raise PackageError(f"package omits required public files: {', '.join(missing)}")
    for prefix in REQUIRED_PREFIXES:
        if not any(entry.startswith(prefix) for entry in entries):
            raise PackageError(f"package contains no {prefix} files")
    forbidden = sorted(
        entry
        for entry in entries
        if any(entry == prefix or entry.startswith(prefix) for prefix in FORBIDDEN_PREFIXES)
    )
    if forbidden:
        raise PackageError(f"package exposes non-product files: {', '.join(forbidden)}")


def public_assets(root: Path) -> set[str]:
    assets = root / "assets"
    if not assets.is_dir():
        return set()
    return {
        path.relative_to(root).as_posix()
        for path in assets.rglob("*")
        if path.is_file()
    }


def package_entries(cargo: str, allow_dirty: bool) -> set[str]:
    command = [cargo, "package", "--locked", "--list"]
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
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PackageError(f"could not list Cargo package: {error}") from error
    if result.returncode != 0:
        raise PackageError(
            f"cargo package --list failed ({result.returncode}):\n{result.stderr}"
        )
    return {line for line in result.stdout.splitlines() if line}


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
    except PackageError as error:
        print(f"package contents error: {error}", file=sys.stderr)
        return 1
    print(f"package contents verified: {len(entries)} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
