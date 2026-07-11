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

PUBLIC_RELEASE_FILES = (
    Path("README.md"),
    Path("CHANGELOG.md"),
    Path("SECURITY.md"),
    Path("docs/architecture.md"),
    Path("herdr-plugin.toml"),
)

FORBIDDEN_PUBLIC_PATTERNS = {
    "release-candidate bookkeeping": re.compile(r"release candidate", re.IGNORECASE),
    "acceptance-hold bookkeeping": re.compile(
        r"acceptance hold|pending human|awaiting explicit .* acceptance",
        re.IGNORECASE,
    ),
    "private planning references": re.compile(
        r"\.(?:omp|hermes)/(?:mission|plans)|START_PROMPT|OPERATING_LOOP|mission evidence|mission complete",
        re.IGNORECASE,
    ),
    "machine-specific details": re.compile(
        r"/(?:home|Users)/[A-Za-z0-9._-]+/(?:repos|src|projects)/|\.ts\.net\b",
        re.IGNORECASE,
    ),
}


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
    cargo_install = re.search(
        r"cargo install[\s\S]{0,200}?--tag (v[0-9]+\.[0-9]+\.[0-9]+)",
        readme,
    )
    if not cargo_install or cargo_install.group(1) != args.tag:
        found = cargo_install.group(1) if cargo_install else None
        fail(f"README.md primary Cargo install tag {found!r} != {args.tag!r}")

    for relative_path in PUBLIC_RELEASE_FILES:
        text = (ROOT / relative_path).read_text(encoding="utf-8")
        for label, pattern in FORBIDDEN_PUBLIC_PATTERNS.items():
            if match := pattern.search(text):
                line = text.count("\n", 0, match.start()) + 1
                fail(f"{relative_path}:{line} contains {label}")

    print(f"release identity verified: {args.tag}")


if __name__ == "__main__":
    main()
