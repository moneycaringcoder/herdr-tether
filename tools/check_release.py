#!/usr/bin/env python3
"""Verify that a release tag matches every public Tether version surface."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import sys
try:
    import tomllib
except ModuleNotFoundError:
    print("release identity error: check_release.py requires Python 3.11+", file=sys.stderr)
    raise SystemExit(1)

ROOT = Path(__file__).resolve().parents[1]


def load_toml(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def fail(message: str) -> None:
    print(f"release identity error: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tag",
        default=os.environ.get("GITHUB_REF_NAME"),
        help="release tag to verify (defaults to GITHUB_REF_NAME)",
    )
    args = parser.parse_args()
    if not args.tag:
        fail("provide --tag v<version> or set GITHUB_REF_NAME")
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", args.tag):
        fail(f"tag {args.tag!r} is not v<major>.<minor>.<patch>")
    version = args.tag[1:]

    cargo_package = load_toml(ROOT / "Cargo.toml")["package"]
    cargo_version = cargo_package["version"]
    plugin_version = load_toml(ROOT / "herdr-plugin.toml")["version"]
    lock_packages = load_toml(ROOT / "Cargo.lock")["package"]
    lock_versions = [
        package["version"]
        for package in lock_packages
        if package["name"] == "herdr-tether"
    ]
    if lock_versions != [version]:
        fail(f"Cargo.lock herdr-tether versions {lock_versions!r} != {version!r}")
    for surface, actual in [
        ("Cargo.toml", cargo_version),
        ("herdr-plugin.toml", plugin_version),
    ]:
        if actual != version:
            fail(f"{surface} version {actual!r} != {version!r}")
    documentation = cargo_package.get("documentation", "")
    if f"/blob/{args.tag}/" not in documentation:
        fail(f"Cargo.toml documentation URL {documentation!r} is not pinned to {args.tag}")

    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    if not re.search(rf"^## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}$", changelog, re.MULTILINE):
        fail(f"CHANGELOG.md has no dated [{version}] release heading")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    if f"--ref {args.tag}" not in readme:
        fail(f"README.md does not pin the primary install to --ref {args.tag}")

    print(f"release identity verified: {args.tag}")


if __name__ == "__main__":
    main()
