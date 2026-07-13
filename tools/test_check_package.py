#!/usr/bin/env python3
import io
import os
from pathlib import Path
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from check_package import (
    PackageError,
    validate_archive_members,
    ArchiveLimits,
    EXPECTED_PACKAGE_FILES,
    extract_validated_archive,
    inspect_archive,
    isolated_build_environment,
    populate_dependency_cache,
    validate_entries,
    validate_lock_packages,
    validate_source_paths,
)


class IsolatedRuntimeEnvironmentTests(unittest.TestCase):
    def test_preserves_default_rustup_home_before_isolating_home(self) -> None:
        with tempfile.TemporaryDirectory() as original, tempfile.TemporaryDirectory() as sandbox:
            rustup_home = Path(original) / ".rustup"
            rustup_home.mkdir()
            with mock.patch.dict(os.environ, {"HOME": original}, clear=False):
                os.environ.pop("RUSTUP_HOME", None)
                environment = isolated_build_environment(
                    Path(sandbox) / "cargo", Path(sandbox) / "home"
                )
            self.assertEqual(environment["RUSTUP_HOME"], str(rustup_home))
            self.assertEqual(environment["HOME"], str(Path(sandbox) / "home"))

    def test_preserves_explicit_rustup_home(self) -> None:
        with tempfile.TemporaryDirectory() as sandbox:
            explicit = str(Path(sandbox) / "custom-rustup")
            with mock.patch.dict(os.environ, {"RUSTUP_HOME": explicit}, clear=False):
                environment = isolated_build_environment(
                    Path(sandbox) / "cargo", Path(sandbox) / "home"
                )
            self.assertEqual(environment["RUSTUP_HOME"], explicit)


class DependencyCacheTests(unittest.TestCase):
    def test_fetches_locked_dependencies_in_repository_environment(self) -> None:
        completed = SimpleNamespace(returncode=0, stdout="", stderr="")
        with mock.patch("check_package.subprocess.run", return_value=completed) as run:
            populate_dependency_cache("/toolchain/bin/cargo")

        run.assert_called_once_with(
            ["/toolchain/bin/cargo", "fetch", "--locked"],
            cwd=mock.ANY,
            check=False,
            text=True,
            stdout=mock.ANY,
            stderr=mock.ANY,
            timeout=180,
        )

    def test_fetch_failure_is_an_actionable_package_error(self) -> None:
        completed = SimpleNamespace(returncode=101, stdout="", stderr="missing crate")
        with (
            mock.patch("check_package.subprocess.run", return_value=completed),
            self.assertRaisesRegex(PackageError, "cargo fetch failed"),
        ):
            populate_dependency_cache("cargo")


class PackageContentsTests(unittest.TestCase):
    def test_complete_public_package_passes(self) -> None:
        validate_entries(
            set(EXPECTED_PACKAGE_FILES),
            {
                "assets/README.md",
                "assets/social-preview.svg",
                "assets/tether-mark.svg",
                "assets/tether-wordmark.svg",
            },
        )

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
            size=1,
            isfile=lambda: True,
            isdir=lambda: False,
        )
        directory = SimpleNamespace(
            name="herdr-tether-0.3.0/src/",
            size=0,
            isfile=lambda: False,
            isdir=lambda: True,
        )
        validate_archive_members([directory, regular], "herdr-tether-0.3.0")
        for malicious in (
            SimpleNamespace(
                name="herdr-tether-0.3.0/../../private",
                size=1,
                isfile=lambda: True,
                isdir=lambda: False,
            ),
            SimpleNamespace(
                name="other-package/src/main.rs",
                size=1,
                isfile=lambda: True,
                isdir=lambda: False,
            ),
            SimpleNamespace(
                name="herdr-tether-0.3.0/src/linked.rs",
                size=0,
                isfile=lambda: False,
                isdir=lambda: False,
            ),
        ):
            with self.subTest(name=malicious.name):
                with self.assertRaises(PackageError):
                    validate_archive_members([malicious], "herdr-tether-0.3.0")

    def test_archive_limits_accept_exact_boundaries_and_reject_n_plus_one(self) -> None:
        prefix = "herdr-tether-0.3.0"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            exact = root / "exact.crate"
            self._write_archive(
                exact,
                [
                    (f"{prefix}/a", b"a" * 4),
                    (f"{prefix}/b", b"b" * 6),
                ],
            )
            compressed = exact.stat().st_size
            limits = ArchiveLimits(
                max_members=2,
                max_member_bytes=6,
                max_total_bytes=10,
                max_archive_bytes=compressed,
            )
            self.assertEqual(inspect_archive(exact, prefix, limits), {"a", "b"})

            cases = (
                (
                    [(f"{prefix}/a", b"a"), (f"{prefix}/b", b"b"), (f"{prefix}/c", b"c")],
                    ArchiveLimits(2, 6, 10, 10_000),
                ),
                (
                    [(f"{prefix}/a", b"a" * 7)],
                    ArchiveLimits(2, 6, 10, 10_000),
                ),
                (
                    [(f"{prefix}/a", b"a" * 5), (f"{prefix}/b", b"b" * 6)],
                    ArchiveLimits(2, 6, 10, 10_000),
                ),
            )
            for index, (members, rejected_limits) in enumerate(cases):
                archive = root / f"n-plus-one-{index}.crate"
                self._write_archive(archive, members)
                with self.subTest(index=index), self.assertRaises(PackageError):
                    inspect_archive(archive, prefix, rejected_limits)

            with self.assertRaises(PackageError):
                inspect_archive(
                    exact,
                    prefix,
                    ArchiveLimits(2, 6, 10, compressed - 1),
                )

    def test_archive_rejects_duplicate_non_normal_and_special_members(self) -> None:
        prefix = "herdr-tether-0.3.0"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            duplicate = root / "duplicate.crate"
            self._write_archive(
                duplicate,
                [(f"{prefix}/src/main.rs", b"first"), (f"{prefix}/src/main.rs", b"second")],
            )
            with self.assertRaises(PackageError):
                inspect_archive(duplicate, prefix)

            for index, name in enumerate(
                (
                    f"{prefix}/src//main.rs",
                    f"{prefix}/src/./main.rs",
                    f"{prefix}/src/../private",
                    f"{prefix}/src/cafe\u0301.rs",
                )
            ):
                archive = root / f"unsafe-{index}.crate"
                self._write_archive(archive, [(name, b"x")])
                with self.subTest(name=name), self.assertRaises(PackageError):
                    inspect_archive(archive, prefix)

            link = root / "link.crate"
            with tarfile.open(link, "w:gz") as package:
                member = tarfile.TarInfo(f"{prefix}/src/link.rs")
                member.type = tarfile.SYMTYPE
                member.linkname = "../../private"
                package.addfile(member)
            with self.assertRaises(PackageError):
                inspect_archive(link, prefix)

    def test_extraction_consumes_validated_inode_when_archive_path_is_replaced(self) -> None:
        prefix = "herdr-tether-0.3.0"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.crate"
            replacement = root / "replacement.crate"
            member = f"{prefix}/src/main.rs"
            self._write_archive(archive, [(member, b"validated inode")])
            self._write_archive(
                replacement, [(f"{prefix}/../escaped", b"replacement inode")]
            )
            real_validate = validate_archive_members

            def replace_after_validation(members, root_prefix, limits=ArchiveLimits()):
                entries = real_validate(members, root_prefix, limits)
                os.replace(replacement, archive)
                return entries

            with mock.patch(
                "check_package.validate_archive_members",
                side_effect=replace_after_validation,
            ):
                extracted = extract_validated_archive(archive, prefix, root / "output")

            self.assertEqual(
                (extracted / "src" / "main.rs").read_bytes(),
                b"validated inode",
            )
            self.assertFalse((root / "output" / "escaped").exists())

    def test_archive_rejects_declared_bomb_before_reading_payload(self) -> None:
        prefix = "herdr-tether-0.3.0"
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "bomb.crate"
            self._write_archive(archive, [(f"{prefix}/bomb", b"x" * 65)])
            with self.assertRaises(PackageError):
                inspect_archive(
                    archive,
                    prefix,
                    ArchiveLimits(2, 64, 64, 10_000),
                )

    @staticmethod
    def _write_archive(path: Path, members: list[tuple[str, bytes]]) -> None:
        with tarfile.open(path, "w:gz") as package:
            for name, payload in members:
                info = tarfile.TarInfo(name)
                info.size = len(payload)
                package.addfile(info, io.BytesIO(payload))

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
