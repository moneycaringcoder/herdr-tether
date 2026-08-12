#!/usr/bin/env python3
"""Offline boundary tests for Herdr upstream canary helpers."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from test_check_herdr_api_contract import contract_schema
from upstream_canary import (
    CanaryError,
    DAILY_LINUX_CRON,
    MAX_INPUT_BYTES,
    WEEKLY_MACOS_CRON,
    parse_version_output,
    resolve_master_sha,
    schema_protocol,
    selected_matrix,
)

HELPER = Path(__file__).with_name("upstream_canary.py")
SHA = "0123456789abcdef0123456789abcdef01234567"


class CanaryHelperTests(unittest.TestCase):
    def test_master_resolution_requires_one_exact_full_lowercase_record(self) -> None:
        self.assertEqual(resolve_master_sha(f"{SHA}\trefs/heads/master\n"), SHA)
        invalid = (
            "",
            f"{SHA[:39]}\trefs/heads/master\n",
            f"{SHA.upper()}\trefs/heads/master\n",
            f"{SHA} refs/heads/master\n",
            f"{SHA}\trefs/heads/main\n",
            f"{SHA}\trefs/heads/master\n{SHA}\trefs/heads/master\n",
        )
        for output in invalid:
            with self.subTest(output=output), self.assertRaises(CanaryError):
                resolve_master_sha(output)

    def test_schedule_and_manual_matrices_are_allowlisted(self) -> None:
        self.assertEqual(
            selected_matrix("schedule", DAILY_LINUX_CRON, ""),
            {
                "include": [
                    {
                        "os": "ubuntu-24.04",
                        "label": "Ubuntu 24.04",
                        "platform": "linux",
                    }
                ]
            },
        )
        self.assertEqual(
            selected_matrix("schedule", WEEKLY_MACOS_CRON, "")["include"][0]["os"],
            "macos-14",
        )
        self.assertEqual(
            [entry["platform"] for entry in selected_matrix(
                "workflow_dispatch", "", "both"
            )["include"]],
            ["linux", "macos"],
        )
        for event, schedule, platform in (
            ("pull_request", "", "linux"),
            ("push", "", "linux"),
            ("schedule", "0 0 * * *", ""),
            ("schedule", DAILY_LINUX_CRON, "linux"),
            ("workflow_dispatch", DAILY_LINUX_CRON, "linux"),
            ("workflow_dispatch", "", "windows"),
        ):
            with self.subTest(event=event, schedule=schedule, platform=platform):
                with self.assertRaises(CanaryError):
                    selected_matrix(event, schedule, platform)

    def test_version_and_protocol_parsing_are_strict(self) -> None:
        self.assertEqual(parse_version_output("herdr 0.8.0\n"), "0.8.0")
        for output in ("0.8.0\n", "herdr v0.8.0\n", "herdr 0.8\n", "herdr 0.8.0 extra\n"):
            with self.subTest(output=output), self.assertRaises(CanaryError):
                parse_version_output(output)
        with tempfile.TemporaryDirectory() as directory:
            schema = Path(directory) / "schema.json"
            schema.write_text(json.dumps(contract_schema(protocol=20)), encoding="utf-8")
            self.assertEqual(schema_protocol(schema), 20)
            schema.write_text(json.dumps(contract_schema(protocol=18)), encoding="utf-8")
            with self.assertRaises(CanaryError):
                schema_protocol(schema)

    def test_cli_input_is_bounded_and_does_not_echo_rejected_data(self) -> None:
        secret = "credential-token-DO-NOT-LEAK"
        result = subprocess.run(
            [sys.executable, str(HELPER), "resolve"],
            input=(secret + "x" * MAX_INPUT_BYTES).encode(),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertNotIn(secret.encode(), result.stderr)
        self.assertLess(len(result.stderr), 256)


if __name__ == "__main__":
    unittest.main()
