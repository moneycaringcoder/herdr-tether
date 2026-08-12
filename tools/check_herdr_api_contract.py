#!/usr/bin/env python3
"""Verify that a Herdr API schema contains Tether's required request surface."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import stat
import sys

MAX_SCHEMA_BYTES = 2 * 1024 * 1024
MIN_PROTOCOL = 19
MAX_PROTOCOL = 2**32 - 1
SCHEMA_VERSION = 1
REQUIRED_METHODS = frozenset(
    {
        "agent.explain",
        "agent.focus",
        "agent.prompt",
        "agent.read",
        "agent.view.clear",
        "agent.view.set",
        "agent.wait",
        "events.subscribe",
        "notification.show",
        "pane.report_metadata",
        "server.agent_manifests",
        "session.snapshot",
        "worktree.list",
    }
)


class ContractError(ValueError):
    pass


def reject_json_constant(value: str) -> None:
    raise ValueError(f"non-standard JSON constant {value}")


def read_schema(path: Path) -> object:
    """Read one regular file descriptor as bounded, strict UTF-8 JSON."""
    flags = os.O_RDONLY
    flags |= getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NONBLOCK", 0)
    descriptor: int | None = None
    try:
        descriptor = os.open(path, flags)
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ContractError("schema input must be a regular file")
        if metadata.st_size > MAX_SCHEMA_BYTES:
            raise ContractError(
                f"schema exceeds {MAX_SCHEMA_BYTES}-byte limit"
            )
        schema_file = os.fdopen(descriptor, "rb")
        descriptor = None
        with schema_file:
            content = schema_file.read(MAX_SCHEMA_BYTES + 1)
    except ContractError:
        raise
    except OSError as error:
        raise ContractError("schema cannot be read") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)
    if len(content) > MAX_SCHEMA_BYTES:
        raise ContractError(f"schema exceeds {MAX_SCHEMA_BYTES}-byte limit")
    try:
        return json.loads(
            content.decode("utf-8", errors="strict"),
            parse_constant=reject_json_constant,
        )
    except (UnicodeError, ValueError, RecursionError) as error:
        raise ContractError("schema is not valid UTF-8 JSON") from error


def require_object(value: object, label: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ContractError(f"{label} must be an object")
    return value


def resolve_local_reference(root: dict[str, object], reference: str) -> object:
    if not reference.startswith("#/"):
        raise ContractError("required request params schema has a non-local reference")
    current: object = root
    for encoded_part in reference[2:].split("/"):
        part = encoded_part.replace("~1", "/").replace("~0", "~")
        if not isinstance(current, dict) or part not in current:
            raise ContractError("required request params schema has an unresolved reference")
        current = current[part]
    return current


def validate_schema(schema: object) -> tuple[int, int]:
    root = require_object(schema, "schema root")
    schema_version = root.get("schema_version")
    if (
        isinstance(schema_version, bool)
        or not isinstance(schema_version, int)
        or schema_version != SCHEMA_VERSION
    ):
        raise ContractError(f"schema_version must be integer {SCHEMA_VERSION}")

    protocol = root.get("protocol")
    if (
        isinstance(protocol, bool)
        or not isinstance(protocol, int)
        or protocol < 0
        or protocol > MAX_PROTOCOL
    ):
        raise ContractError("schema protocol must be an unsigned 32-bit integer")
    if protocol < MIN_PROTOCOL:
        raise ContractError(
            f"schema protocol {protocol} is older than Tether minimum {MIN_PROTOCOL}"
        )

    schemas = require_object(root.get("schemas"), "schemas")
    request = require_object(schemas.get("request"), "schemas.request")
    variants = request.get("oneOf")
    if not isinstance(variants, list):
        raise ContractError("schemas.request.oneOf must be an array")

    method_variants: dict[str, list[dict[str, object]]] = {}
    for variant in variants:
        if not isinstance(variant, dict):
            continue
        properties = variant.get("properties")
        if not isinstance(properties, dict):
            continue
        method_schema = properties.get("method")
        if not isinstance(method_schema, dict):
            continue
        method = method_schema.get("const")
        if isinstance(method, str):
            method_variants.setdefault(method, []).append(properties)

    missing = sorted(REQUIRED_METHODS.difference(method_variants))
    if missing:
        raise ContractError(
            "missing required request methods: " + ", ".join(missing)
        )
    for method in sorted(REQUIRED_METHODS):
        matches = method_variants[method]
        if len(matches) != 1:
            raise ContractError(
                f"required request method {method} is defined more than once"
            )
        params = matches[0].get("params")
        if not isinstance(params, dict):
            raise ContractError(
                f"required request method {method} has no params schema"
            )
        resolved_params: object = params
        if "$ref" in params:
            reference = params["$ref"]
            if not isinstance(reference, str):
                raise ContractError(
                    f"required request method {method} has an invalid params reference"
                )
            try:
                resolved_params = resolve_local_reference(root, reference)
            except ContractError as error:
                raise ContractError(
                    f"required request method {method} params schema is not resolvable"
                ) from error
        if not isinstance(resolved_params, dict) or resolved_params.get("type") != "object":
            raise ContractError(
                f"required request method {method} params schema must describe an object"
            )
    return protocol, len(REQUIRED_METHODS)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("schema", type=Path, help="path to herdr-api.schema.json")
    args = parser.parse_args()
    try:
        protocol, method_count = validate_schema(read_schema(args.schema))
    except ContractError as error:
        print(f"Herdr API contract error: {error}", file=sys.stderr)
        raise SystemExit(1)
    print(
        f"Herdr API contract verified: protocol {protocol}; "
        f"{method_count} required methods"
    )


if __name__ == "__main__":
    main()
