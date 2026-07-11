#!/usr/bin/env python3
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

from check_package import (
    PackageError,
    validate_archive_members,
    validate_entries,
    validate_lock_packages,
    validate_source_paths,
)


class PackageContentsTests(unittest.TestCase):
    def test_complete_public_package_passes(self) -> None:
        entries = {
            ".cargo_vcs_info.json",
            "Cargo.lock",
            "Cargo.toml",
            "Cargo.toml.orig",
            "CHANGELOG.md",
            "CONTRIBUTING.md",
            "LICENSE",
            "README.md",
            "SECURITY.md",
            "docs/architecture.md",
            "assets/tether-mark.svg",
            "herdr-plugin.toml",
            "src/main.rs",
            "tests/cli.rs",
        }
        validate_entries(entries, {"assets/tether-mark.svg"})

    def test_private_and_ci_files_are_rejected(self) -> None:
        baseline = {
            "Cargo.lock",
            "Cargo.toml",
            "CHANGELOG.md",
            "CONTRIBUTING.md",
            "LICENSE",
            "README.md",
            "SECURITY.md",
            "docs/architecture.md",
            "herdr-plugin.toml",
            "src/main.rs",
        }
        for leaked in (".omp/mission/private.txt", ".github/workflows/release.yml", "tools/check_release.py"):
            with self.subTest(leaked=leaked):
                with self.assertRaises(PackageError):
                    validate_entries(baseline | {leaked}, set())

    def test_unlisted_top_level_and_malicious_paths_are_rejected(self) -> None:
        baseline = {
            "Cargo.lock",
            "Cargo.toml",
            "CHANGELOG.md",
            "CONTRIBUTING.md",
            "LICENSE",
            "README.md",
            "SECURITY.md",
            "docs/architecture.md",
            "herdr-plugin.toml",
            "src/main.rs",
        }
        for path in (
            "secrets.txt",
            "../outside",
            "src/../../.omp/mission/private.txt",
            "/absolute/path",
            r"src\..\private",
        ):
            with self.subTest(path=path):
                with self.assertRaises(PackageError):
                    validate_entries(baseline | {path}, set())

    def test_source_symlinks_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "outside").write_text("private", encoding="utf-8")
            os.symlink(root / "outside", root / "src" / "linked.rs")
            with self.assertRaises(PackageError):
                validate_source_paths(root, {"src/linked.rs"})

    def test_archive_rejects_traversal_and_link_members(self) -> None:
        regular = SimpleNamespace(
            name="herdr-tether-0.3.0/src/main.rs",
            isfile=lambda: True,
            isdir=lambda: False,
        )
        directory = SimpleNamespace(
            name="herdr-tether-0.3.0/src/",
            isfile=lambda: False,
            isdir=lambda: True,
        )
        validate_archive_members([directory, regular], "herdr-tether-0.3.0")
        for malicious in (
            SimpleNamespace(
                name="herdr-tether-0.3.0/../../private",
                isfile=lambda: True,
                isdir=lambda: False,
            ),
            SimpleNamespace(
                name="other-package/src/main.rs",
                isfile=lambda: True,
                isdir=lambda: False,
            ),
            SimpleNamespace(
                name="herdr-tether-0.3.0/src/linked.rs",
                isfile=lambda: False,
                isdir=lambda: False,
            ),
        ):
            with self.subTest(name=malicious.name):
                with self.assertRaises(PackageError):
                    validate_archive_members([malicious], "herdr-tether-0.3.0")

    def test_missing_manifest_doc_or_asset_is_rejected(self) -> None:
        baseline = {
            "Cargo.lock",
            "Cargo.toml",
            "CHANGELOG.md",
            "CONTRIBUTING.md",
            "LICENSE",
            "README.md",
            "SECURITY.md",
            "docs/architecture.md",
            "herdr-plugin.toml",
            "src/main.rs",
        }
        for missing in ("LICENSE", "README.md", "herdr-plugin.toml"):
            with self.subTest(missing=missing):
                with self.assertRaises(PackageError):
                    validate_entries(baseline - {missing}, set())
        with self.assertRaises(PackageError):
            validate_entries(baseline, {"assets/tether-mark.svg"})


    def test_locked_registry_dependencies_require_checksums(self) -> None:
        packages = [
            {"name": "herdr-tether", "version": "0.3.0"},
            {
                "name": "serde",
                "version": "1.0.228",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "checksum": "a" * 64,
            },
        ]
        validate_lock_packages(packages)
        for mutation in (
            {"source": "git+https://example.invalid/repository", "checksum": "a" * 64},
            {
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "checksum": None,
            },
        ):
            changed = [dict(package) for package in packages]
            changed[1].update(mutation)
            with self.subTest(mutation=mutation):
                with self.assertRaises(PackageError):
                    validate_lock_packages(changed)

    def test_only_the_root_package_may_have_no_source(self) -> None:
        with self.assertRaises(PackageError):
            validate_lock_packages(
                [
                    {"name": "herdr-tether", "version": "0.3.0"},
                    {"name": "local-dependency", "version": "1.0.0"},
                ]
            )

if __name__ == "__main__":
    unittest.main()
