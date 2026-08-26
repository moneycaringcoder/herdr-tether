#!/usr/bin/env python3
import re
import unittest

from check_release import (
    PUBLIC_RELEASE_FILES,
    ROOT,
    ReleaseIdentityError,
    changelog_notes,
    load_toml,
    resolve_target_tag,
    validate_readme_install,
    validate_hermes_install,
    validate_release_context,
)


class ChangelogNotesTests(unittest.TestCase):
    CHANGELOG = """\
# Changelog

## [0.7.3] - 2026-08-22

### Added

- The newest thing.

## [0.7.2] - 2026-08-16

- The older thing.
"""

    def test_notes_are_one_section_without_the_next_one(self) -> None:
        self.assertEqual(
            changelog_notes(self.CHANGELOG, "0.7.3"),
            "### Added\n\n- The newest thing.\n",
        )

    def test_the_last_section_ends_at_the_end_of_the_file(self) -> None:
        self.assertEqual(
            changelog_notes(self.CHANGELOG, "0.7.2"), "- The older thing.\n"
        )

    def test_a_section_saying_only_headings_is_refused(self) -> None:
        empty = self.CHANGELOG.replace("- The newest thing.\n", "")
        with self.assertRaisesRegex(ReleaseIdentityError, "says nothing"):
            changelog_notes(empty, "0.7.3")

    def test_an_undated_heading_is_named_as_the_reason(self) -> None:
        undated = self.CHANGELOG.replace("## [0.7.3] - 2026-08-22", "## [0.7.3]")
        with self.assertRaisesRegex(ReleaseIdentityError, "no ISO-8601 date"):
            changelog_notes(undated, "0.7.3")

    def test_a_missing_section_is_refused(self) -> None:
        with self.assertRaisesRegex(ReleaseIdentityError, r"no dated \[0\.8\.0\]"):
            changelog_notes(self.CHANGELOG, "0.8.0")

    def test_a_fenced_block_can_hold_a_line_that_looks_like_a_heading(self) -> None:
        fenced = self.CHANGELOG.replace(
            "- The newest thing.\n",
            "- The newest thing.\n\n```\n## not a section\n```\n",
        )
        notes = changelog_notes(fenced, "0.7.3")
        self.assertIn("## not a section", notes)
        self.assertNotIn("0.7.2", notes)

    def test_an_unclosed_fence_is_refused_rather_than_swallowing_the_file(self) -> None:
        unclosed = self.CHANGELOG.replace(
            "- The newest thing.\n", "- The newest thing.\n\n```\n"
        )
        with self.assertRaisesRegex(ReleaseIdentityError, "code fence open"):
            changelog_notes(unclosed, "0.7.3")

    def test_the_repository_changelog_publishes_the_current_version(self) -> None:
        version = load_toml(ROOT / "Cargo.toml")["package"]["version"]
        text = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
        notes = changelog_notes(text, version)
        self.assertTrue(notes.endswith("\n"))
        self.assertIn(notes.strip("\n"), text)
        for heading in re.findall(r"^## \[.+\](?: - .+)?$", text, re.MULTILINE):
            self.assertNotIn(heading, notes)


class TargetTagTests(unittest.TestCase):
    def test_candidate_derives_the_package_tag(self) -> None:
        self.assertEqual(
            resolve_target_tag(
                candidate=True,
                explicit_tag=None,
                release=False,
                package_version="0.7.1",
            ),
            "v0.7.1",
        )

    def test_matching_explicit_tag_is_accepted(self) -> None:
        self.assertEqual(
            resolve_target_tag(
                candidate=False,
                explicit_tag="v0.7.1",
                release=True,
                package_version="0.7.1",
            ),
            "v0.7.1",
        )

    def test_environment_tag_remains_the_implicit_non_candidate_fallback(self) -> None:
        self.assertEqual(
            resolve_target_tag(
                candidate=False,
                explicit_tag=None,
                environment_tag="v0.7.1",
                release=False,
                package_version="0.7.1",
            ),
            "v0.7.1",
        )
        self.assertEqual(
            resolve_target_tag(
                candidate=True,
                explicit_tag=None,
                environment_tag="pull/55/merge",
                release=False,
                package_version="0.7.1",
            ),
            "v0.7.1",
        )

    def test_target_mode_must_be_unambiguous(self) -> None:
        invalid = (
            dict(candidate=False, explicit_tag=None, release=False),
            dict(candidate=True, explicit_tag="v0.7.1", release=False),
            dict(candidate=True, explicit_tag=None, release=True),
        )
        for selection in invalid:
            with self.subTest(selection=selection):
                with self.assertRaises(ReleaseIdentityError):
                    resolve_target_tag(package_version="0.7.1", **selection)

    def test_tag_and_package_version_must_be_stable_semver_and_match(self) -> None:
        invalid = (
            dict(explicit_tag="0.7.1", package_version="0.7.1"),
            dict(explicit_tag="v0.7.0", package_version="0.7.1"),
            dict(explicit_tag="v0.7.1", package_version="0.7.1-rc.1"),
            dict(explicit_tag="v0.7.1", package_version="00.7.1"),
            dict(explicit_tag="v0.7.1", package_version="0.07.1"),
            dict(explicit_tag="v0.7.1", package_version="0.7.01"),
            dict(explicit_tag="v0.7.1", package_version=701),
        )
        for identity in invalid:
            with self.subTest(identity=identity):
                with self.assertRaises(ReleaseIdentityError):
                    resolve_target_tag(
                        candidate=False,
                        release=False,
                        **identity,
                    )


class ReadmeInstallIdentityTests(unittest.TestCase):
    def test_candidate_accepts_main_only_or_complete_stable_install_paths(self) -> None:
        main_only = """\
herdr plugin install moneycaringcoder/herdr-tether --ref main
Use a full commit SHA for a reproducible candidate checkout.
"""
        complete = """\
Stable v0.3.0:
herdr plugin install moneycaringcoder/herdr-tether --ref v0.3.0
cargo install --git https://github.com/moneycaringcoder/herdr-tether \
  --tag v0.3.0 --locked herdr-tether
"""
        validate_readme_install(main_only, "v0.3.0", release=False)
        validate_readme_install(complete, "v0.3.0", release=False)

    def test_candidate_rejects_partial_stable_or_unpublished_copy(self) -> None:
        for text in (
            "herdr plugin install owner/repo --ref v0.3.0",
            "cargo install --git https://example.invalid/repo --tag v0.3.0 --locked",
            """\
Stable v0.3.0 (once published). After `v0.3.0` is published:
herdr plugin install owner/repo --ref v0.3.0
cargo install --git https://example.invalid/repo --tag v0.3.0 --locked
""",
        ):
            with self.subTest(text=text):
                with self.assertRaises(ReleaseIdentityError):
                    validate_readme_install(text, "v0.3.0", release=False)

    def test_quickstart_is_a_checked_public_release_surface(self) -> None:
        self.assertIn("docs/quickstart.md", {str(path) for path in PUBLIC_RELEASE_FILES})

    def test_hermes_skill_is_a_checked_public_release_surface(self) -> None:
        self.assertIn(
            "integrations/hermes/SKILL.md",
            {str(path) for path in PUBLIC_RELEASE_FILES},
        )

    def test_compatibility_matrix_is_a_checked_public_release_surface(self) -> None:
        self.assertIn(
            "docs/compatibility.md",
            {str(path) for path in PUBLIC_RELEASE_FILES},
        )

    def test_hermes_install_requires_stable_main_and_exact_sha_paths(self) -> None:
        base = "https://raw.githubusercontent.com/moneycaringcoder/herdr-tether"
        complete = f"""\
Stable:
{base}/v0.3.0/integrations/hermes/SKILL.md
Development:
{base}/main/integrations/hermes/SKILL.md
Immutable:
TETHER_SKILL_REF=FULL_COMMIT_SHA_YOU_REVIEWED
{base}/${{TETHER_SKILL_REF}}/integrations/hermes/SKILL.md
"""
        validate_hermes_install(complete, "v0.3.0", surface="test")
        for required in (
            f"{base}/v0.3.0/integrations/hermes/SKILL.md",
            f"{base}/main/integrations/hermes/SKILL.md",
            "TETHER_SKILL_REF=FULL_COMMIT_SHA_YOU_REVIEWED",
            f"{base}/${{TETHER_SKILL_REF}}/integrations/hermes/SKILL.md",
        ):
            with self.subTest(required=required):
                with self.assertRaises(ReleaseIdentityError):
                    validate_hermes_install(
                        complete.replace(required, ""),
                        "v0.3.0",
                        surface="test",
                    )

    def test_release_requires_both_tagged_install_paths(self) -> None:
        complete = """\
herdr plugin install moneycaringcoder/herdr-tether --ref v0.3.0 --yes
cargo install --git https://github.com/moneycaringcoder/herdr-tether \\
  --tag v0.3.0 --locked herdr-tether
"""
        validate_readme_install(complete, "v0.3.0", release=True)
        with self.assertRaises(ReleaseIdentityError):
            validate_readme_install(
                "herdr plugin install owner/repo --ref v0.3.0 --yes",
                "v0.3.0",
                release=True,
            )
        future_copy = complete.replace(
            "herdr plugin install",
            "Stable v0.3.0 (once published). v0.3.0 is not published yet.\n"
            "herdr plugin install",
            1,
        )
        with self.assertRaises(ReleaseIdentityError):
            validate_readme_install(future_copy, "v0.3.0", release=True)


    def test_release_requires_exact_local_and_github_tag_context(self) -> None:
        tag = "v0.3.0"
        commit = "a" * 40
        validate_release_context(
            tag,
            head_commit=commit,
            tagged_commit=commit,
            github_actions=False,
            github_ref_type=None,
            github_ref_name=None,
        )
        invalid_contexts = (
            dict(head_commit="b" * 40, tagged_commit=commit),
            dict(head_commit=commit, tagged_commit=None),
        )
        for context in invalid_contexts:
            with self.subTest(context=context):
                with self.assertRaises(ReleaseIdentityError):
                    validate_release_context(
                        tag,
                        github_actions=False,
                        github_ref_type=None,
                        github_ref_name=None,
                        **context,
                    )
        with self.assertRaises(ReleaseIdentityError):
            validate_release_context(
                tag,
                head_commit=commit,
                tagged_commit=commit,
                github_actions=True,
                github_ref_type="branch",
                github_ref_name="main",
            )


if __name__ == "__main__":
    unittest.main()
