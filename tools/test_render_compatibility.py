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
            arch: x86_64
            platform: linux
            herdr_version: "0.8.0"
          - os: macos-14
            label: macOS 14
            arch: arm64
            platform: macos
            herdr_version: "0.9.1"
    runs-on: ${{ matrix.os }}
    steps:
      - name: Something with a label: in it
        run: echo
  other-job:
    strategy:
      matrix:
        include:
          - os: ubuntu-22.04
            label: Ubuntu 22.04
            arch: x86_64
            herdr_version: "0.6.0"
"""

CANARY = """
on:
  schedule:
    - cron: "23 5 * * *"
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
    (root / render_compatibility.CANARY_GATE).write_text(CANARY, encoding="utf-8")
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
        self.assertIn("| Ubuntu 24.04 | x86_64 | 0.8.0 |", document)
        self.assertIn("| macOS 14 | arm64 | 0.9.1 |", document)
        self.assertIn("this revision of Tether was exercised", document)
        # Both releases, not just the one the protocol floor names.
        self.assertIn("other than 0.8.0, 0.9.1 are not covered", document)
        # A second job in the same file has its own matrix, and its platforms
        # were never through this gate.
        self.assertNotIn("Ubuntu 22.04", document)

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

    def test_an_entry_a_yaml_edit_reshapes_is_an_error_not_a_missing_row(self):
        # A silently dropped entry publishes a table that understates what was
        # exercised, which is worse than a failed check: every gate stays green.
        reshaped = [
            GATE.replace(
                "            label: Ubuntu 24.04\n            platform: linux\n",
                "            platform: linux\n            label: Ubuntu 24.04\n",
            ),
            GATE.replace('herdr_version: "0.8.0"', "herdr_version: '0.8.0'"),
            GATE.replace('herdr_version: "0.8.0"', "herdr_version: 0.8.0"),
            GATE.replace(
                "          - os: ubuntu-24.04\n",
                "          - os: ubuntu-24.04\n            # the LTS runner\n",
            ),
        ]
        for gate in reshaped:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                write_tree(root, gate=gate)
                document = render_compatibility.render(root)
            self.assertIn("Ubuntu 24.04", document, "the reshaped entry must survive")
            self.assertIn("macOS 14", document)

    def test_an_entry_without_a_label_or_a_version_is_an_error(self):
        for removed in ("            label: macOS 14\n", '            herdr_version: "0.9.1"\n'):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                write_tree(root, gate=GATE.replace(removed, ""))
                with self.assertRaises(render_compatibility.CompatibilityError):
                    render_compatibility.render(root)

    def test_a_decorated_label_does_not_reach_the_table_verbatim(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root, gate=GATE.replace("label: Ubuntu 24.04", 'label: "Ubuntu 24.04" # LTS'))
            document = render_compatibility.render(root)
        self.assertIn("| Ubuntu 24.04 | x86_64 | 0.8.0 |", document)
        self.assertNotIn("LTS", document)

    def test_a_label_that_would_break_the_table_is_an_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root, gate=GATE.replace("label: macOS 14", "label: macOS | 14"))
            with self.assertRaises(render_compatibility.CompatibilityError):
                render_compatibility.render(root)

    def test_a_gate_without_the_live_product_job_is_an_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root, gate=GATE.replace("  live-product:", "  renamed-job:"))
            with self.assertRaises(render_compatibility.CompatibilityError):
                render_compatibility.render(root)

    def test_a_canary_without_a_schedule_is_an_error(self):
        # The document says the canary runs on a schedule, so the claim is
        # checked rather than asserted.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_tree(root)
            (root / render_compatibility.CANARY_GATE).write_text(
                "on:\n  workflow_dispatch:\n", encoding="utf-8"
            )
            with self.assertRaises(render_compatibility.CompatibilityError):
                render_compatibility.render(root)

    def test_the_shipped_document_lists_every_platform_the_real_gate_exercises(self):
        # Self-consistency between the renderer and its own output cannot catch
        # the renderer being wrong about this repository, so pin the rows a user
        # actually reads against the gate's own entry count.
        root = Path(__file__).resolve().parents[1]
        gate = (root / render_compatibility.STABLE_GATE).read_text(encoding="utf-8")
        document = (root / render_compatibility.DOCUMENT).read_text(encoding="utf-8")
        entries = render_compatibility.parse_stable_gate(gate)
        rows = [line for line in document.splitlines() if line.startswith("| ") and " | 0." in line]
        self.assertEqual(len(rows), len(entries))
        for label, arch, version in entries:
            self.assertIn(f"| {label} | {arch} | {version} |", document)
        self.assertIn("Ubuntu 24.04", document)
        self.assertIn("macOS 14", document)
        # Every architecture the gate names reaches the table, so a reader on one
        # of them does not have to infer coverage from an operating system name.
        for arch in {arch for _, arch, _ in entries}:
            self.assertIn(f"| {arch} | ", document)


    def test_every_asset_the_gate_can_select_is_reachable_by_a_job(self):
        # The gate used to carry asset branches for architectures no job ran on,
        # which made it read as broader coverage than it had. Pin the two lists
        # against each other so a branch cannot outlive its runner, and a runner
        # cannot be added without its asset.
        root = Path(__file__).resolve().parents[1]
        gate = (root / render_compatibility.STABLE_GATE).read_text(encoding="utf-8")
        entries = render_compatibility.parse_stable_gate(gate)
        block = render_compatibility.job_block(gate, render_compatibility.STABLE_GATE_JOB)

        selectable = set()
        for line in block.splitlines():
            stripped = line.strip()
            if not stripped.endswith(")") or ":" not in stripped or stripped.startswith("#"):
                continue
            for candidate in stripped.removesuffix(")").split("|"):
                parts = candidate.strip().split(":")
                if len(parts) == 3 and parts[0][0].isdigit():
                    selectable.add(tuple(parts))
        self.assertTrue(selectable, "the asset selection was not found")

        platforms = {}
        for line in block.splitlines():
            stripped = line.strip()
            if stripped.startswith("- os:") or stripped.startswith("platform:") or stripped.startswith("arch:") or stripped.startswith("herdr_version:"):
                key, _, value = stripped.removeprefix("- ").partition(":")
                platforms.setdefault(key.strip(), []).append(
                    value.strip().strip('"')
                )
        reachable = {
            (version, platform, arch)
            for version, platform, arch in zip(
                platforms["herdr_version"], platforms["platform"], platforms["arch"]
            )
        }
        # `uname -m` reports arm64 on macOS and aarch64 on Linux, which is why
        # each entry carries the architecture its runner actually reports. The two
        # sets must match exactly: a branch with no runner claims coverage the
        # gate does not have, and a runner with no branch fails the download.
        self.assertEqual(selectable, reachable)
        self.assertEqual(len(entries), len(reachable))

if __name__ == "__main__":
    unittest.main()
