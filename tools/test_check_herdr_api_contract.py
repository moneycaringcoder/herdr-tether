#!/usr/bin/env python3
"""Boundary tests for Tether's required Herdr API contract."""

from __future__ import annotations

import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

from check_herdr_api_contract import (
    ContractError,
    MAX_SCHEMA_BYTES,
    REQUIRED_METHODS,
    read_schema,
    validate_schema,
)

CHECKER = Path(__file__).with_name("check_herdr_api_contract.py")


def request_variant(method: str) -> dict[str, object]:
    return {
        "properties": {
            "method": {"const": method, "type": "string"},
            "params": {"type": "object"},
        },
        "required": ["method", "params"],
        "type": "object",
    }


def contract_schema(protocol: object = 19) -> dict[str, object]:
    return {
        "protocol": protocol,
        "schema_version": 1,
        "schemas": {
            "request": {
                "oneOf": [request_variant(method) for method in sorted(REQUIRED_METHODS)]
            }
        },
    }


class ContractValidationTests(unittest.TestCase):
    def test_supported_contract_is_verified(self) -> None:
        self.assertEqual(validate_schema(contract_schema()), (19, 13))

    def test_new_protocol_and_unrelated_additions_are_tolerated(self) -> None:
        schema = contract_schema(protocol=20)
        schema["future_root"] = {"anything": True}
        schemas = schema["schemas"]
        assert isinstance(schemas, dict)
        schemas["future_schema"] = [1, 2, 3]
        request = schemas["request"]
        assert isinstance(request, dict)
        request["future_request_key"] = "ignored"
        variants = request["oneOf"]
        assert isinstance(variants, list)
        variants.append(request_variant("pane.input.set"))
        first = variants[0]
        assert isinstance(first, dict)
        first["future_variant_key"] = {"nested": "ignored"}
        self.assertEqual(validate_schema(schema), (20, 13))

    def test_every_required_method_is_enforced(self) -> None:
        for missing in sorted(REQUIRED_METHODS):
            with self.subTest(missing=missing):
                schema = contract_schema()
                request = schema["schemas"]["request"]
                request["oneOf"] = [
                    variant
                    for variant in request["oneOf"]
                    if variant["properties"]["method"]["const"] != missing
                ]
                with self.assertRaisesRegex(
                    ContractError,
                    f"missing required request methods: {re.escape(missing)}",
                ):
                    validate_schema(schema)

    def test_required_method_must_be_unique_and_have_params(self) -> None:
        duplicate = contract_schema()
        duplicate["schemas"]["request"]["oneOf"].append(
            request_variant("agent.prompt")
        )
        with self.assertRaisesRegex(ContractError, "defined more than once"):
            validate_schema(duplicate)

        missing_params = contract_schema()
        for variant in missing_params["schemas"]["request"]["oneOf"]:
            if variant["properties"]["method"]["const"] == "agent.prompt":
                del variant["properties"]["params"]
        with self.assertRaisesRegex(ContractError, "has no params schema"):
            validate_schema(missing_params)

    def test_params_schema_must_resolve_to_an_object(self) -> None:
        invalid_params = (
            ({"$ref": "https://example.invalid/params"}, "not resolvable"),
            ({"$ref": "#/schemas/request/$defs/Missing"}, "not resolvable"),
            ({"$ref": 7}, "invalid params reference"),
            ({"type": "null"}, "must describe an object"),
        )
        for params, message in invalid_params:
            with self.subTest(params=params):
                schema = contract_schema()
                for variant in schema["schemas"]["request"]["oneOf"]:
                    if variant["properties"]["method"]["const"] == "agent.prompt":
                        variant["properties"]["params"] = params
                with self.assertRaisesRegex(ContractError, message):
                    validate_schema(schema)

    def test_root_version_protocol_and_request_shapes_fail_closed(self) -> None:
        invalid = (
            ([], "schema root must be an object"),
            ({}, "schema_version must be integer 1"),
            ({"schema_version": True}, "schema_version must be integer 1"),
            (contract_schema(protocol=True), "unsigned 32-bit integer"),
            (contract_schema(protocol=2**32), "unsigned 32-bit integer"),
            (contract_schema(protocol=18), "older than Tether minimum 19"),
        )
        for schema, message in invalid:
            with self.subTest(message=message), self.assertRaisesRegex(
                ContractError, message
            ):
                validate_schema(schema)

        missing_variants = contract_schema()
        missing_variants["schemas"]["request"]["oneOf"] = {}
        with self.assertRaisesRegex(ContractError, "oneOf must be an array"):
            validate_schema(missing_variants)


class ContractInputTests(unittest.TestCase):
    def run_checker(self, content: bytes) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            schema = Path(directory) / "schema.json"
            schema.write_bytes(content)
            return subprocess.run(
                [sys.executable, str(CHECKER), str(schema)],
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

    def test_cli_has_deterministic_success_output(self) -> None:
        result = self.run_checker(json.dumps(contract_schema()).encode())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            "Herdr API contract verified: protocol 19; 13 required methods\n",
        )
        self.assertEqual(result.stderr, "")

    def test_malformed_and_non_utf8_json_are_rejected_without_echoing_input(self) -> None:
        for content in (
            b"{not-json secret-value",
            b'"\xffsecret-value"',
            b'{"protocol": NaN, "secret": "secret-value"}',
            b'{"protocol": ' + b"9" * 5000 + b', "secret": "secret-value"}',
        ):
            with self.subTest(content=content):
                result = self.run_checker(content)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(
                    result.stderr,
                    "Herdr API contract error: schema is not valid UTF-8 JSON\n",
                )
                self.assertNotIn("secret-value", result.stderr)

    def test_oversized_regular_file_is_rejected_before_json_parsing(self) -> None:
        result = self.run_checker(b"{" + b" " * MAX_SCHEMA_BYTES)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stderr,
            f"Herdr API contract error: schema exceeds {MAX_SCHEMA_BYTES}-byte limit\n",
        )

    def test_read_schema_rejects_a_directory_without_disclosing_its_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ContractError, "regular file") as raised:
                read_schema(Path(directory))
            self.assertNotIn(directory, str(raised.exception))


if __name__ == "__main__":
    unittest.main()
