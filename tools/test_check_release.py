#!/usr/bin/env python3
import unittest

from check_release import (
    PUBLIC_RELEASE_FILES,
    ReleaseIdentityError,
    validate_readme_install,
    validate_hermes_install,
    validate_release_context,
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
