#!/usr/bin/env python3
"""Boundary tests for the public documentation contract checker."""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

CHECKER = Path(__file__).with_name("check_docs.py")
SPEC = importlib.util.spec_from_file_location("check_docs", CHECKER)
assert SPEC is not None and SPEC.loader is not None
check_docs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_docs)


class DocsCheckerTests(unittest.TestCase):
    def run_check(self, files: dict[str, str], symlinks: dict[str, str] | None = None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, text in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            for name, target in (symlinks or {}).items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.symlink_to(target)
            return subprocess.run(
                [sys.executable, str(CHECKER), "--root", str(root)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

    def assert_ok(self, files, symlinks=None):
        result = self.run_check(files, symlinks)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "documentation contracts: ok\n")

    def test_relative_links_images_queries_fragments_and_skip_policy(self):
        self.assert_ok({
            "README.md": r"""# Home
[guide](docs/guide.md?view=full#caf%C3%A9)
![logo](assets/logo.svg "Logo")
[absolute](/host/path) [web](https://example.invalid/x) [mail](mailto:a@example.invalid)
[escaped \[label\]](docs/guide.md#explicit)
```md
[not a link](missing.md)
```
""",
            "docs/guide.md": "# Café\n<a id=\"explicit\"></a>\n",
            "assets/logo.svg": "<svg/>",
        })

    def test_missing_file_and_anchor_are_sorted_and_percent_decoded(self):
        result = self.run_check({
            "README.md": "[z](z.md) [anchor](guide.md#does%20not%20exist) [a](a.md)\n",
            "guide.md": "# Present\n",
        })
        self.assertEqual(result.returncode, 1)
        lines = result.stdout.splitlines()
        self.assertEqual(lines, sorted(lines))
        self.assertIn("link-anchor README.md: missing anchor 'does not exist' in guide.md", lines)
        self.assertIn("link-file README.md: missing relative target 'a.md'", lines)
        self.assertIn("link-file README.md: missing relative target 'z.md'", lines)
        self.assertNotIn(str(Path.cwd()), result.stdout)

    def test_duplicate_heading_suffixes_and_explicit_anchors(self):
        self.assert_ok({
            "README.md": "[one](guide.md#same) [two](guide.md#same-1) [named](guide.md#named)\n",
            "guide.md": "# Same\n## Same\n<a name='named'></a>\n",
        })

    def test_duplicate_explicit_anchor_is_rejected(self):
        result = self.run_check({"README.md": "<a id='same'></a>\n<a name=\"same\"></a>\n"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("anchor-duplicate README.md: duplicate explicit anchor 'same'", result.stdout)

    def test_control_characters_in_fragments_and_explicit_anchors_are_redacted(self):
        control = "\u202e"
        result = self.run_check(
            {
                "README.md": (
                    f"[fragment](guide.md#unsafe{control}value)\n"
                    f"<a id='unsafe{control}value'></a>\n"
                ),
                "guide.md": "# Safe\n",
            }
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("anchor-invalid README.md:", result.stdout)
        self.assertIn("link-invalid README.md:", result.stdout)
        self.assertNotIn(control, result.stdout)

    def test_traversal_and_encoded_traversal_fail_closed(self):
        result = self.run_check({"README.md": "[plain](../secret) [encoded](%2e%2e/secret)\n"})
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout.count("link-traversal README.md:"), 2)
        self.assertNotIn("secret`", result.stdout)

    @unittest.skipIf(not hasattr(os, "symlink"), "symlinks unavailable")
    def test_symlink_escape_fails_closed(self):
        with tempfile.TemporaryDirectory() as outside:
            target = Path(outside) / "outside.md"
            target.write_text("# Outside\n", encoding="utf-8")
            result = self.run_check(
                {"README.md": "[outside](docs/outside.md)\n"},
                {"docs/outside.md": str(target)},
            )
        self.assertEqual(result.returncode, 1)
        self.assertIn("link-traversal README.md: relative target escapes documentation root", result.stdout)
        self.assertNotIn(outside, result.stdout)

    def test_case_mismatch_is_named(self):
        result = self.run_check({"README.md": "[guide](docs/guide.md)\n", "docs/Guide.md": "# Guide\n"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("link-case README.md: target case does not match 'docs/Guide.md'", result.stdout)

    def test_escaped_parenthesis_is_not_regex_parsed(self):
        self.assert_ok({"README.md": r"[odd](docs/a\(b\).md)\n", "docs/a(b).md": "# Odd\n"})

    def test_canonical_mismatches_name_category_and_public_file(self):
        result = self.run_check({
            "Cargo.toml": '[package]\nversion = "1.2.3"\nrust-version = "1.88"\n',
            "herdr-plugin.toml": 'version = "1.2.4"\nmin_herdr_version = "0.7.3"\n',
            "README.md": "**Stable v1.2.2.**\n**Development (`main`).** not a release\nRust 1.87 or newer; `tmux` 3.3 or newer; Herdr 0.7.3 or newer\n",
            "docs/quickstart.md": "**Stable v1.2.3.**\n**Development (`main`).** not a release\nRust 1.88 or newer; `tmux` 3.3 or newer; Herdr 0.7.3 or newer\n",
        })
        self.assertEqual(result.returncode, 1)
        self.assertIn("package-version README.md:", result.stdout)
        self.assertIn("package-version herdr-plugin.toml:", result.stdout)
        self.assertIn("rust-requirement README.md:", result.stdout)
        self.assertNotIn(str(Path.cwd()), result.stdout)

    def test_config_defaults_and_capability_contracts(self):
        files = {
            "Cargo.toml": '[package]\nversion = "1.2.3"\nrust-version = "1.88"\n',
            "herdr-plugin.toml": 'version = "1.2.3"\nmin_herdr_version = "0.7.3"\n',
            "README.md": "**Stable v1.2.3.** **Development (`main`).** not a release\nRust 1.88 or newer; `tmux` 3.3 or newer; Herdr 0.7.3 or newer\n",
            "docs/quickstart.md": "**Stable v1.2.3.** **Development (`main`).** not a release\nRust 1.88 or newer; `tmux` 3.3 or newer; Herdr 0.7.3 or newer\n",
            "src/config.rs": "pub const CURRENT_VERSION: u32 = 2;\nplacement: Placement::SplitRight,\nmax_depth: 4,\nmax_entries: 4096,\nmax_results: 64,\ntimeout_seconds: 3,\nworkers: 4,\nclosed_days: 30\n",
            "docs/configuration.md": "```toml\nversion = 2\nhosts = []\n[ui]\nplacement = \"split-right\"\n[discovery]\nlocal_roots = []\nmax_depth = 4\nmax_entries = 4096\nmax_results = 64\ntimeout_seconds = 3\nworkers = 4\n[retention]\nclosed_days = 30\n```\n- `split-right` (default)\n",
            "src/orchestration.rs": "observe_output: true,\nopen_interactive: true,\n",
            "docs/architecture.md": "UI defaults grant both bounded observation and interactive open to selected workers.\n",
        }
        self.assert_ok(files)
        files["docs/configuration.md"] = files["docs/configuration.md"].replace("workers = 4", "workers = 5")
        files["docs/architecture.md"] = "UI defaults grant bounded observation only.\n"
        result = self.run_check(files)
        self.assertEqual(result.returncode, 1)
        self.assertIn("config-default docs/configuration.md: documented workers must be 4", result.stdout)
        self.assertIn("capability-default docs/architecture.md:", result.stdout)

    def test_read_public_uses_opened_inode_when_path_is_replaced(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "README.md"
            path.write_text("# original\n", encoding="utf-8")
            replacement = root / "replacement.md"
            replacement.write_text("# replacement\n", encoding="utf-8")
            opened_inode: list[int] = []
            opened_binary: list[bool] = []
            original_open = Path.open

            def replace_after_open(candidate, *args, **kwargs):
                handle = original_open(candidate, *args, **kwargs)
                opened_inode.append(os.fstat(handle.fileno()).st_ino)
                opened_binary.append(isinstance(handle.read(0), bytes))
                os.replace(replacement, path)
                self.assertNotEqual(opened_inode[-1], path.stat().st_ino)
                return handle

            findings = check_docs.Findings()
            with mock.patch.object(Path, "open", replace_after_open):
                text = check_docs.read_public(path, root, findings)

        self.assertEqual(text, "# original\n")
        self.assertEqual(opened_binary, [True])
        self.assertEqual(findings.output(), [])

    def test_canonical_reads_use_opened_inode_when_path_is_replaced(self):
        canonical = (
            "Cargo.toml", "herdr-plugin.toml", "src/config.rs",
            "docs/configuration.md", "src/orchestration.rs", "docs/architecture.md",
        )
        for relative in canonical:
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("original", encoding="utf-8")
                replacement = root / "replacement"
                replacement.write_text("replacement", encoding="utf-8")
                original_open = Path.open

                def replace_after_open(candidate, *args, **kwargs):
                    handle = original_open(candidate, *args, **kwargs)
                    os.replace(replacement, path)
                    self.assertNotEqual(os.fstat(handle.fileno()).st_ino, path.stat().st_ino)
                    return handle

                with mock.patch.object(Path, "open", replace_after_open):
                    self.assertEqual(check_docs.read_bounded_utf8(path), "original")

    def test_canonical_reads_reject_exactly_limit_plus_one(self):
        canonical = (
            "Cargo.toml", "herdr-plugin.toml", "src/config.rs",
            "docs/configuration.md", "src/orchestration.rs", "docs/architecture.md",
        )
        for relative in canonical:
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"x" * (check_docs.MAX_DOCUMENT_BYTES + 1))
                with self.assertRaises(check_docs.BoundedTextTooLarge):
                    check_docs.read_bounded_utf8(path)

    def test_canonical_reads_reject_growth_past_limit_after_open(self):
        canonical = (
            "Cargo.toml", "herdr-plugin.toml", "src/config.rs",
            "docs/configuration.md", "src/orchestration.rs", "docs/architecture.md",
        )
        for relative in canonical:
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"small")
                original_open = Path.open

                def grow_after_open(candidate, *args, **kwargs):
                    handle = original_open(candidate, *args, **kwargs)
                    with original_open(path, "ab") as writer:
                        writer.write(b"x" * check_docs.MAX_DOCUMENT_BYTES)
                    return handle

                with mock.patch.object(Path, "open", grow_after_open):
                    with self.assertRaises(check_docs.BoundedTextTooLarge):
                        check_docs.read_bounded_utf8(path)

    def test_production_checker_has_no_direct_path_read_text(self):
        self.assertNotIn(".read_text(", CHECKER.read_text(encoding="utf-8"))

    def test_read_public_rejects_file_grown_past_limit_after_open(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "README.md"
            path.write_bytes(b"small")
            original_open = Path.open

            def grow_after_open(candidate, *args, **kwargs):
                handle = original_open(candidate, *args, **kwargs)
                with open(path, "ab") as writer:
                    writer.write(b"x" * check_docs.MAX_DOCUMENT_BYTES)
                return handle

            findings = check_docs.Findings()
            with mock.patch.object(Path, "open", grow_after_open):
                text = check_docs.read_public(path, root, findings)

        self.assertIsNone(text)
        self.assertEqual(
            findings.output(),
            [f"document-size README.md: exceeds {check_docs.MAX_DOCUMENT_BYTES} bytes"],
        )

    def test_diagnostics_are_bounded(self):
        links = " ".join(f"[x{i}](missing-{i}.md)" for i in range(300))
        result = self.run_check({"README.md": links})
        self.assertEqual(result.returncode, 1)
        self.assertLessEqual(len(result.stdout.splitlines()), 101)
        self.assertIn("diagnostic-limit <root>:", result.stdout)


if __name__ == "__main__":
    unittest.main()
