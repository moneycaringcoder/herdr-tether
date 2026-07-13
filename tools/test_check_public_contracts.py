#!/usr/bin/env python3
import io
import os
from pathlib import Path
import stat
import tarfile
import tempfile
import unittest
from unittest import mock

from check_public_contracts import (
    ArchiveLimits,
    ContractError,
    extract_cli_examples,
    inspect_archive,
    validate_cli_examples,
)


PREFIX = "herdr-tether-0.3.0"


def make_archive(path: Path, files: dict[str, bytes]) -> None:
    with tarfile.open(path, "w:gz") as archive:
        root = tarfile.TarInfo(PREFIX)
        root.type = tarfile.DIRTYPE
        archive.addfile(root)
        for name, contents in files.items():
            member = tarfile.TarInfo(f"{PREFIX}/{name}")
            member.size = len(contents)
            archive.addfile(member, io.BytesIO(contents))


class PackageDerivedPublicScanTests(unittest.TestCase):
    def test_scans_every_text_member_but_safely_classifies_binary_assets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "package.crate"
            make_archive(
                archive,
                {
                    "README.md": b"public text\n",
                    "docs/nested.md": b"token = ghp_abcdefghijklmnopqrstuvwxyz1234567890\n",
                    "assets/image.bin": b"\x89PNG\r\n\x1a\n\x00\xff",
                },
            )
            with self.assertRaisesRegex(ContractError, r"docs/nested\.md: credential-token") as caught:
                inspect_archive(archive)
            self.assertNotIn("ghp_", str(caught.exception))

    def test_detects_seeded_private_and_residue_patterns_without_echoing_them(self) -> None:
        seeds = {
            "private-path": "/home/secret-user/project",
            "private-host": "secret-user@workstation.local",
            "private-key": "-----BEGIN OPENSSH PRIVATE KEY-----",
            "unicode-format": "safe\u202eevil",
            "orchestration-residue": "internal mission ledger transcript",
            "unsafe-release-wording": "curl installer | sh",
        }
        for expected, seed in seeds.items():
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as directory:
                archive = Path(directory) / "package.crate"
                make_archive(archive, {"docs/public.md": seed.encode()})
                with self.assertRaises(ContractError) as caught:
                    inspect_archive(archive)
                diagnostic = str(caught.exception)
                self.assertIn(expected, diagnostic)
                self.assertNotIn(seed, diagnostic)
                self.assertLessEqual(len(diagnostic), 2048)

    def test_diagnostics_are_sorted_bounded_and_do_not_include_secret_values(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "package.crate"
            files = {
                f"docs/{index:03}.md": f"api_token = SECRET_VALUE_{index:03}".encode()
                for index in range(100)
            }
            make_archive(archive, files)
            with self.assertRaises(ContractError) as caught:
                inspect_archive(archive, max_diagnostics=8)
            message = str(caught.exception)
            self.assertNotIn("SECRET_VALUE", message)
            self.assertLessEqual(message.count("credential-token"), 8)
            paths = [part.split(": ", 1)[0] for part in message.split("; ") if part.startswith("docs/")]
            self.assertEqual(paths, sorted(paths))


class ArchiveSafetyTests(unittest.TestCase):
    def test_rejects_each_archive_budget_at_n_plus_one(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = (
                ("members", {"README.md": b"a", "docs/a.md": b"b"}, ArchiveLimits(max_members=2)),
                ("member", {"README.md": b"abc"}, ArchiveLimits(max_member_bytes=2)),
                ("total", {"README.md": b"ab", "docs/a.md": b"cd"}, ArchiveLimits(max_total_bytes=3)),
            )
            for name, files, limits in cases:
                with self.subTest(name=name):
                    archive = root / f"{name}.crate"
                    make_archive(archive, files)
                    with self.assertRaises(ContractError):
                        inspect_archive(archive, limits=limits)

    def test_rejects_compressed_and_decompressed_bombs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "bomb.crate"
            make_archive(archive, {"README.md": b"x" * 4096})
            compressed_size = archive.stat().st_size
            with self.assertRaises(ContractError):
                inspect_archive(archive, limits=ArchiveLimits(max_archive_bytes=compressed_size - 1))
            with self.assertRaises(ContractError):
                inspect_archive(archive, limits=ArchiveLimits(max_member_bytes=8192, max_total_bytes=8192, max_decompressed_bytes=512))

    def test_rejects_unsafe_paths_and_special_types(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index, name in enumerate((f"{PREFIX}/../escape.md", f"{PREFIX}/docs\\evil.md")):
                archive = root / f"path-{index}.crate"
                with tarfile.open(archive, "w:gz") as output:
                    member = tarfile.TarInfo(name)
                    member.size = 1
                    output.addfile(member, io.BytesIO(b"x"))
                with self.assertRaises(ContractError):
                    inspect_archive(archive)
            special = root / "special.crate"
            with tarfile.open(special, "w:gz") as output:
                root_member = tarfile.TarInfo(PREFIX)
                root_member.type = tarfile.DIRTYPE
                output.addfile(root_member)
                link = tarfile.TarInfo(f"{PREFIX}/docs/link.md")
                link.type = tarfile.SYMTYPE
                link.linkname = "README.md"
                output.addfile(link)
            with self.assertRaises(ContractError):
                inspect_archive(special)

    def test_rejects_symlink_replacement_at_atomic_open(self) -> None:
        if not hasattr(os, "O_NOFOLLOW"):
            self.skipTest("platform does not expose O_NOFOLLOW")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.crate"
            target = root / "target.crate"
            make_archive(archive, {"README.md": b"safe\n"})
            make_archive(target, {"README.md": b"api_token = SECRET_VALUE\n"})
            real_open = os.open
            swapped = False

            def replacing_open(path: object, flags: int, *args: object, **kwargs: object) -> int:
                nonlocal swapped
                if Path(path) == archive and not swapped:
                    swapped = True
                    archive.unlink()
                    archive.symlink_to(target)
                return real_open(path, flags, *args, **kwargs)

            with mock.patch.object(os, "open", replacing_open):
                with self.assertRaisesRegex(ContractError, "could not be inspected"):
                    inspect_archive(archive)
            self.assertTrue(swapped)

    def test_consumes_the_same_descriptor_that_was_validated(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "package.crate"
            replacement = root / "replacement.crate"
            make_archive(archive, {"README.md": b"safe\n"})
            make_archive(replacement, {"README.md": b"api_token = SECRET_VALUE\n"})
            real_open = os.open
            swapped = False

            def replacing_open(path: object, flags: int, *args: object, **kwargs: object) -> int:
                nonlocal swapped
                descriptor = real_open(path, flags, *args, **kwargs)
                if Path(path) == archive and not swapped:
                    swapped = True
                    os.replace(replacement, archive)
                return descriptor

            with mock.patch.object(os, "open", replacing_open):
                self.assertEqual(inspect_archive(archive), [])
            self.assertTrue(swapped)


class MalformedPublicTextTests(unittest.TestCase):
    def test_docs_and_config_text_fail_closed_on_nul_or_invalid_utf8(self) -> None:
        malformed = (b"safe\x00hidden", b"safe\xffhidden")
        surfaces = ("docs/public.md", "README.md", "herdr-plugin.toml", "config.json")
        for surface in surfaces:
            for contents in malformed:
                with self.subTest(surface=surface, contents=contents), tempfile.TemporaryDirectory() as directory:
                    archive = Path(directory) / "package.crate"
                    make_archive(archive, {surface: contents})
                    with self.assertRaisesRegex(ContractError, "malformed-public-text"):
                        inspect_archive(archive)

    def test_known_binary_asset_may_be_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "package.crate"
            make_archive(archive, {"assets/image.png": b"\x89PNG\r\n\x1a\n\x00\xff"})
            self.assertEqual(inspect_archive(archive), [])


class CuratedCliExampleTests(unittest.TestCase):
    def test_extracts_only_tether_commands_and_preserves_release_reference_kinds(self) -> None:
        markdown = """\
```sh
herdr-tether orchestration list --json
cargo install --git https://github.com/moneycaringcoder/herdr-tether --tag v0.3.0 --locked herdr-tether
cargo install --git https://github.com/moneycaringcoder/herdr-tether --branch main --locked herdr-tether
cargo install --git https://github.com/moneycaringcoder/herdr-tether --rev 0123456789abcdef0123456789abcdef01234567 --locked herdr-tether
```
"""
        examples = extract_cli_examples(markdown, "README.md")
        self.assertEqual([example.kind for example in examples], ["cli", "stable", "main", "exact-sha"])

    def test_rejects_shell_features_instead_of_executing_them(self) -> None:
        unsafe = (
            "herdr-tether doctor | cat",
            "herdr-tether doctor > out",
            "herdr-tether doctor $(id)",
            "TOKEN=value herdr-tether doctor",
            "herdr-tether doctor; echo bad",
        )
        for command in unsafe:
            with self.subTest(command=command):
                markdown = f"```sh\n{command}\n```\n"
                with self.assertRaises(ContractError):
                    extract_cli_examples(markdown, "README.md")

    def test_runs_non_mutating_help_contract_against_supplied_binary(self) -> None:
        markdown = "```console\n$ herdr-tether host add build example.net --root /srv/repos\n$ herdr-tether doctor\n```\n"
        examples = extract_cli_examples(markdown, "docs/example.md")
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "herdr-tether"
            log = Path(directory) / "argv.log"
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "open(os.environ['ARGV_LOG'], 'a').write(' '.join(sys.argv[1:]) + '\\n')\n"
                "raise SystemExit(0 if sys.argv[-1:] == ['--help'] else 9)\n",
                encoding="utf-8",
            )
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            validate_cli_examples(examples, binary, environment={"ARGV_LOG": str(log)})
            invocations = log.read_text(encoding="utf-8").splitlines()
            self.assertEqual(invocations, ["host add --help", "doctor --help"])


if __name__ == "__main__":
    unittest.main()
