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
    Path("docs/compatibility.md"),
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


STABLE_VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def resolve_target_tag(
    *,
    candidate: bool,
    explicit_tag: str | None,
    environment_tag: str | None = None,
    release: bool,
    package_version: object,
) -> str:
    """Resolve one release target without duplicating the package version."""
    if candidate and explicit_tag is not None:
        raise ReleaseIdentityError("choose exactly one of --candidate or --tag")
    if not candidate and explicit_tag is None:
        explicit_tag = environment_tag
    if not candidate and explicit_tag is None:
        raise ReleaseIdentityError(
            "provide --candidate, --tag v<version>, or GITHUB_REF_NAME"
        )
    if candidate and release:
        raise ReleaseIdentityError("--candidate cannot be combined with --release")
    if not isinstance(package_version, str) or not re.fullmatch(
        STABLE_VERSION_PATTERN, package_version
    ):
        raise ReleaseIdentityError(
            f"Cargo.toml package version {package_version!r} is not <major>.<minor>.<patch>"
        )

    package_tag = f"v{package_version}"
    if candidate:
        return package_tag
    if not re.fullmatch(rf"v{STABLE_VERSION_PATTERN}", explicit_tag or ""):
        raise ReleaseIdentityError(
            f"tag {explicit_tag!r} is not v<major>.<minor>.<patch>"
        )
    if explicit_tag != package_tag:
        raise ReleaseIdentityError(
            f"tag {explicit_tag!r} does not match Cargo.toml version {package_version!r}"
        )
    return explicit_tag


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
    if future_copy.search(readme):
        raise ReleaseIdentityError(f"{surface} still describes {tag} as unpublished")
    if release or herdr_tag in readme or cargo_tag:
        if herdr_tag not in readme:
            raise ReleaseIdentityError(
                f"{surface} does not pin the primary install to {herdr_tag}"
            )
        if not cargo_tag:
            raise ReleaseIdentityError(
                f"{surface} does not pin the primary Cargo install to --tag {tag}"
            )

def validate_hermes_install(text: str, tag: str, *, surface: str) -> None:
    skill_path = "integrations/hermes/SKILL.md"
    base = "https://raw.githubusercontent.com/moneycaringcoder/herdr-tether"
    required = {
        "stable tag": f"{base}/{tag}/{skill_path}",
        "development main": f"{base}/main/{skill_path}",
        "immutable exact SHA variable": "TETHER_SKILL_REF=FULL_COMMIT_SHA_YOU_REVIEWED",
        "immutable exact SHA URL": f"{base}/${{TETHER_SKILL_REF}}/{skill_path}",
    }
    for label, value in required.items():
        if value not in text:
            raise ReleaseIdentityError(
                f"{surface} does not provide the Hermes {label} install path"
            )
    for label in ("Stable", "Development", "Immutable"):
        if label not in text:
            raise ReleaseIdentityError(
                f"{surface} does not clearly label the Hermes {label.lower()} install"
            )


def changelog_notes(text: str, version: str) -> str:
    """The body of one dated changelog section, as the release notes for it.

    Publication reads the notes from the same checkout every other gate ran
    against, so what a release page says is what the tagged tree says.
    """
    heading = re.compile(
        rf"^## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}$", re.MULTILINE
    )
    match = heading.search(text)
    if match is None:
        if re.search(rf"^## \[{re.escape(version)}\]", text, re.MULTILINE):
            raise ReleaseIdentityError(
                f"CHANGELOG.md has a [{version}] heading with no ISO-8601 date on it"
            )
        raise ReleaseIdentityError(f"CHANGELOG.md has no dated [{version}] release heading")
    rest = text[match.end() :]
    body: list[str] = []
    fence: str | None = None
    for line in rest.splitlines():
        if fence is None:
            # A fenced block can hold anything, including a line that looks like
            # the next section, so the section ends at a heading only out here.
            if line.startswith("## "):
                break
            if match_fence := re.match(r"(```+|~~~+)", line):
                fence = match_fence.group(1)[:3]
        elif line.startswith(fence):
            fence = None
        body.append(line)
    if fence is not None:
        raise ReleaseIdentityError(
            f"CHANGELOG.md section for [{version}] leaves a code fence open"
        )
    # A section holding only sub-headings is empty to a reader, and publishing it
    # produces a release page that says "### Added" and nothing else.
    if not any(line.strip() and not line.lstrip().startswith("#") for line in body):
        raise ReleaseIdentityError(f"CHANGELOG.md section for [{version}] says nothing")
    return "\n".join(body).strip("\n") + "\n"


def fail(message: str) -> None:
    print(f"release identity error: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tag",
        help="explicit release tag to verify (defaults to GITHUB_REF_NAME outside candidate mode)",
    )
    parser.add_argument(
        "--candidate",
        action="store_true",
        help="derive the candidate tag from Cargo.toml",
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help="enforce tagged release install instructions (candidate mode is the default)",
    )
    parser.add_argument(
        "--notes-out",
        type=Path,
        help="write this version's changelog section here once every check passes",
    )
    args = parser.parse_args()
    cargo_package = load_toml(ROOT / "Cargo.toml")["package"]
    cargo_version = cargo_package["version"]
    try:
        tag = resolve_target_tag(
            candidate=args.candidate,
            explicit_tag=args.tag,
            environment_tag=os.environ.get("GITHUB_REF_NAME"),
            release=args.release,
            package_version=cargo_version,
        )
    except ReleaseIdentityError as error:
        fail(str(error))
    version = tag[1:]
    if args.release:
        try:
            head_commit = resolve_git_commit("HEAD")
            if head_commit is None:
                raise ReleaseIdentityError("could not resolve Git HEAD")
            validate_release_context(
                tag,
                head_commit=head_commit,
                tagged_commit=resolve_git_commit(f"refs/tags/{tag}"),
                github_actions=os.environ.get("GITHUB_ACTIONS") == "true",
                github_ref_type=os.environ.get("GITHUB_REF_TYPE"),
                github_ref_name=os.environ.get("GITHUB_REF_NAME"),
            )
        except ReleaseIdentityError as error:
            fail(str(error))

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
    valid_documentation_refs = (tag,) if args.release else ("main", tag)
    if not any(
        f"/blob/{reference}/" in documentation for reference in valid_documentation_refs
    ):
        expected = " or ".join(valid_documentation_refs)
        fail(
            f"Cargo.toml documentation URL {documentation!r} is not pinned to "
            f"{expected}"
        )

    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    try:
        notes = changelog_notes(changelog, version)
    except ReleaseIdentityError as error:
        fail(str(error))
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    try:
        validate_readme_install(readme, tag, args.release)
    except ReleaseIdentityError as error:
        fail(str(error))
    quickstart_path = Path("docs/quickstart.md")
    try:
        validate_readme_install(
            (ROOT / quickstart_path).read_text(encoding="utf-8"),
            tag,
            args.release,
            surface=str(quickstart_path),
        )
    except ReleaseIdentityError as error:
        fail(str(error))
    hermes_path = Path("integrations/hermes/SKILL.md")
    for relative_path in (Path("README.md"), hermes_path):
        try:
            validate_hermes_install(
                (ROOT / relative_path).read_text(encoding="utf-8"),
                tag,
                surface=str(relative_path),
            )
        except ReleaseIdentityError as error:
            fail(str(error))

    for relative_path in PUBLIC_RELEASE_FILES:
        text = (ROOT / relative_path).read_text(encoding="utf-8")
        for label, pattern in FORBIDDEN_PUBLIC_PATTERNS.items():
            if match := pattern.search(text):
                line = text.count("\n", 0, match.start()) + 1
                fail(f"{relative_path}:{line} contains {label}")

    if args.notes_out is not None:
        try:
            args.notes_out.write_text(notes, encoding="utf-8")
        except OSError as error:
            fail(f"notes could not be written: {error}")

    identity = "release" if args.release else "candidate"
    print(f"{identity} identity verified: {tag}")


if __name__ == "__main__":
    main()
