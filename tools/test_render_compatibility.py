#!/usr/bin/env python3
"""Tests for the Herdr compatibility matrix renderer."""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import render_compatibility  # noqa: E402

GATE = """
jobs:
  live-product:
    strategy:
      matrix:
        include:
          - os: ubuntu-24.04
            label: Ubuntu 24.04
            platform: linux
            herdr_version: "0.8.0"
          - os: macos-14
            label: macOS 14
            platform: macos
            herdr_version: "0.9.1"
"""

SOURCE = """
pub const MIN_HERDR_PROTOCOL: u32 = 19;
pub const MIN_HERDR_VERSION_LABEL: &str = "0.8.0";
pub(crate) const MAX_AUDITED_HERDR_PROTOCOL: u32 = 20;
"""


def write_tree(root: Path, *, gate: str = GATE, source: str = SOURCE, minimum: str = "0.8.0"):
    (root / ".github/workflows").mkdir(parents=True, exist_ok=True)
    (root / "src").mkdir(parents=True, exist_ok=True)
    (root / "docs").mkdir(parents=True, exist_ok=True)
    (root / render_compatibility.STABLE_GATE).write_text(gate, encoding="utf-8")
    (root / render_compatibility.PROTOCOL_SOURCE).write_text(source, encoding="utf-8")
    (root / render_compatibility.PLUGIN_MANIFEST).write_text(
        f'min_herdr_version = "{minimum}"\nversion = "0.7.2"\n', encoding="utf-8"
    )
    (root / render_compatibility.PACKAGE_MANIFEST).write_text(
        '[package]\nversion = "0.7.2"\n', encoding="utf-8"
    )


class RenderCompatibilityTest(unittest.TestCase):
    def test_every_exercised_platform_and_release_reaches_the_table(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root)
            document = render_compatibility.render(root)
        self.assertIn("| Ubuntu 24.04 | 0.8.0 |", document)
        self.assertIn("| macOS 14 | 0.9.1 |", document)
        self.assertIn("Tether 0.7.2 was exercised", document)
        # Both releases, not just the one the protocol floor names.
        self.assertIn("other than 0.8.0, 0.9.1 are not covered", document)

    def test_protocol_numbers_come_from_the_source_rather_than_the_prose(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(
                root,
                source=SOURCE.replace("= 19", "= 21").replace("= 20", "= 23"),
                minimum="0.8.0",
            )
            document = render_compatibility.render(root)
        self.assertIn("from protocol 21", document)
        self.assertIn("through protocol 23", document)

    def test_a_gate_with_no_platforms_is_an_error_not_an_empty_table(self):
        # An empty table would read as "exercised against nothing", which is a
        # worse claim than a failed check.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root, gate="jobs:\n  live-product:\n    runs-on: ubuntu-24.04\n")
            with self.assertRaises(render_compatibility.CompatibilityError):
                render_compatibility.render(root)

    def test_a_manifest_minimum_that_contradicts_the_source_is_an_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root, minimum="0.7.0")
            with self.assertRaises(render_compatibility.CompatibilityError):
                render_compatibility.render(root)

    def test_a_missing_protocol_constant_is_an_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root, source="pub const MIN_HERDR_PROTOCOL: u32 = 19;\n")
            with self.assertRaises(render_compatibility.CompatibilityError):
                render_compatibility.render(root)

    def test_check_reports_a_missing_document(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root)
            findings = render_compatibility.check(root)
        self.assertEqual(len(findings), 1)
        self.assertIn("is missing", findings[0])

    def test_check_reports_a_document_that_no_longer_matches_its_gates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root)
            (root / render_compatibility.DOCUMENT).write_text(
                render_compatibility.render(root), encoding="utf-8"
            )
            self.assertEqual(render_compatibility.check(root), [])
            # The gate starts exercising a newer Herdr; the tracked document is
            # now a claim nobody verified.
            (root / render_compatibility.STABLE_GATE).write_text(
                GATE.replace('"0.9.1"', '"0.10.0"'), encoding="utf-8"
            )
            findings = render_compatibility.check(root)
        self.assertEqual(len(findings), 1)
        self.assertIn("does not match the gates", findings[0])

    def test_writing_the_document_makes_the_check_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root)
            self.assertEqual(render_compatibility.main(["--root", str(root), "--write"]), 0)
            self.assertEqual(render_compatibility.main(["--root", str(root)]), 0)

    def test_the_repository_document_matches_its_gates(self):
        root = Path(__file__).resolve().parents[1]
        self.assertEqual(render_compatibility.check(root), [])


if __name__ == "__main__":
    unittest.main()
