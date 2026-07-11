#!/usr/bin/env python3
import unittest

from check_release import (
    ReleaseIdentityError,
    validate_readme_install,
    validate_release_context,
)


class ReadmeInstallIdentityTests(unittest.TestCase):
    def test_candidate_rejects_nonexistent_version_tag_commands(self) -> None:
        truthful = """\
herdr plugin install moneycaringcoder/herdr-tether --ref main --yes
Use a full commit SHA for a reproducible candidate checkout.
"""
        validate_readme_install(truthful, "v0.3.0", release=False)
        for command in (
            "herdr plugin install owner/repo --ref v0.3.0 --yes",
            "cargo install --git https://example.invalid/repo --tag v0.3.0 --locked",
        ):
            with self.subTest(command=command):
                with self.assertRaises(ReleaseIdentityError):
                    validate_readme_install(truthful + command, "v0.3.0", release=False)

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
