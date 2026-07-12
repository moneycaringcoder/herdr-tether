#!/usr/bin/env python3
"""Verify that a release tag matches every public Tether version surface."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import subprocess
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
    Path("docs/quickstart.md"),
    Path("integrations/hermes/SKILL.md"),
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


class ReleaseIdentityError(ValueError):
    pass


def validate_release_context(
    tag: str,
    *,
    head_commit: str,
    tagged_commit: str | None,
    github_actions: bool,
    github_ref_type: str | None,
    github_ref_name: str | None,
) -> None:
    if tagged_commit is None or tagged_commit != head_commit:
        raise ReleaseIdentityError(
            f"--release requires HEAD to be the exact local {tag} tag"
        )
    if github_actions and (
        github_ref_type != "tag" or github_ref_name != tag
    ):
        raise ReleaseIdentityError(
            "--release must run from the exact GitHub tag being validated"
        )


def resolve_git_commit(ref: str) -> str | None:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--verify", f"{ref}^{{commit}}"],
            cwd=ROOT,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseIdentityError(f"could not inspect Git release context: {error}") from error
    if result.returncode != 0:
        return None
    commit = result.stdout.strip()
    if not re.fullmatch(r"[0-9a-f]{40,64}", commit):
        raise ReleaseIdentityError(f"Git returned invalid commit identity {commit!r}")
    return commit


def validate_readme_install(
    readme: str,
    tag: str,
    release: bool,
    *,
    surface: str = "README.md",
) -> None:
    herdr_tag = f"--ref {tag}"
    cargo_tag = re.search(
        rf"cargo install[\s\S]{{0,200}}?--tag {re.escape(tag)}(?:\s|$)",
        readme,
    )
    future_copy = re.compile(
        rf"(?:is\s+not\s+published\s+yet|once\s+published|"
        rf"after\s+`?{re.escape(tag)}`?\s+is\s+published)",
        re.IGNORECASE,
    )
    if release:
        if future_copy.search(readme):
            raise ReleaseIdentityError(
                f"{surface} still describes {tag} as unpublished"
            )
        if herdr_tag not in readme:
            raise ReleaseIdentityError(
                f"{surface} does not pin the primary install to {herdr_tag}"
            )
        if not cargo_tag:
            raise ReleaseIdentityError(
                f"{surface} does not pin the primary Cargo install to --tag {tag}"
            )
    elif herdr_tag in readme or cargo_tag:
        tag_references = [
            match.start()
            for match in re.finditer(
                rf"(?:--ref|--tag)\s+{re.escape(tag)}(?:\s|$)",
                readme,
            )
        ]
        future_label = re.compile(
            rf"after\s+`?{re.escape(tag)}`?\s+is\s+published",
            re.IGNORECASE,
        )
        if any(
            not future_label.search(readme[max(0, position - 400) : position])
            for position in tag_references
        ):
            raise ReleaseIdentityError(
                f"{surface} advertises nonexistent candidate tag {tag}; "
                f"label tagged commands for use after {tag} is published"
            )


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
    parser.add_argument(
        "--release",
        action="store_true",
        help="enforce tagged release install instructions (candidate mode is the default)",
    )
    args = parser.parse_args()
    if not args.tag:
        fail("provide --tag v<version> or set GITHUB_REF_NAME")
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", args.tag):
        fail(f"tag {args.tag!r} is not v<major>.<minor>.<patch>")
    version = args.tag[1:]
    if args.release:
        try:
            head_commit = resolve_git_commit("HEAD")
            if head_commit is None:
                raise ReleaseIdentityError("could not resolve Git HEAD")
            validate_release_context(
                args.tag,
                head_commit=head_commit,
                tagged_commit=resolve_git_commit(f"refs/tags/{args.tag}"),
                github_actions=os.environ.get("GITHUB_ACTIONS") == "true",
                github_ref_type=os.environ.get("GITHUB_REF_TYPE"),
                github_ref_name=os.environ.get("GITHUB_REF_NAME"),
            )
        except ReleaseIdentityError as error:
            fail(str(error))

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
    expected_documentation_ref = args.tag if args.release else "main"
    if f"/blob/{expected_documentation_ref}/" not in documentation:
        fail(
            f"Cargo.toml documentation URL {documentation!r} is not pinned to "
            f"{expected_documentation_ref}"
        )

    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    if not re.search(rf"^## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}$", changelog, re.MULTILINE):
        fail(f"CHANGELOG.md has no dated [{version}] release heading")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    try:
        validate_readme_install(readme, args.tag, args.release)
    except ReleaseIdentityError as error:
        fail(str(error))
    quickstart_path = Path("docs/quickstart.md")
    try:
        validate_readme_install(
            (ROOT / quickstart_path).read_text(encoding="utf-8"),
            args.tag,
            args.release,
            surface=str(quickstart_path),
        )
    except ReleaseIdentityError as error:
        fail(str(error))

    for relative_path in PUBLIC_RELEASE_FILES:
        text = (ROOT / relative_path).read_text(encoding="utf-8")
        for label, pattern in FORBIDDEN_PUBLIC_PATTERNS.items():
            if match := pattern.search(text):
                line = text.count("\n", 0, match.start()) + 1
                fail(f"{relative_path}:{line} contains {label}")

    identity = "release" if args.release else "candidate"
    print(f"{identity} identity verified: {args.tag}")


if __name__ == "__main__":
    main()
