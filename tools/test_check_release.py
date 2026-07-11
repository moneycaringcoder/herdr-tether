#!/usr/bin/env python3
import unittest

from check_release import ReleaseIdentityError, validate_readme_install


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


if __name__ == "__main__":
    unittest.main()
