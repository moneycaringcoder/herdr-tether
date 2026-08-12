#!/usr/bin/env python3
"""Strict offline helpers for the advisory Herdr upstream canary."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys

from check_herdr_api_contract import ContractError, read_schema, validate_schema

MAX_INPUT_BYTES = 4096
MASTER_REF = "refs/heads/master"
DAILY_LINUX_CRON = "23 5 * * *"
WEEKLY_MACOS_CRON = "41 5 * * 0"
FULL_SHA = re.compile(r"[0-9a-f]{40}")
VERSION_LINE = re.compile(r"herdr ([0-9]+\.[0-9]+\.[0-9]+)\n?")
PLATFORMS = {
    "linux": {"os": "ubuntu-24.04", "label": "Ubuntu 24.04", "platform": "linux"},
    "macos": {"os": "macos-14", "label": "macOS 14", "platform": "macos"},
}


class CanaryError(ValueError):
    pass


def bounded_stdin() -> str:
    content = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
    if len(content) > MAX_INPUT_BYTES:
        raise CanaryError(f"input exceeds {MAX_INPUT_BYTES}-byte limit")
    try:
        return content.decode("utf-8", errors="strict")
    except UnicodeError as error:
        raise CanaryError("input is not valid UTF-8") from error


def resolve_master_sha(output: str) -> str:
    lines = output.splitlines()
    if len(lines) != 1:
        raise CanaryError("expected exactly one ls-remote record")
    fields = lines[0].split("\t")
    if len(fields) != 2 or fields[1] != MASTER_REF or not FULL_SHA.fullmatch(fields[0]):
        raise CanaryError("ls-remote did not return one full lowercase master SHA")
    return fields[0]


def parse_version_output(output: str) -> str:
    match = VERSION_LINE.fullmatch(output)
    if match is None:
        raise CanaryError("Herdr version output has an unexpected shape")
    return match.group(1)


def selected_matrix(event_name: str, schedule: str, platform: str) -> dict[str, object]:
    if event_name == "schedule":
        if platform:
            raise CanaryError("scheduled runs must not provide a platform input")
        if schedule == DAILY_LINUX_CRON:
            selected = ["linux"]
        elif schedule == WEEKLY_MACOS_CRON:
            selected = ["macos"]
        else:
            raise CanaryError("schedule is not an approved canary cadence")
    elif event_name == "workflow_dispatch":
        if schedule:
            raise CanaryError("manual runs must not provide a schedule")
        if platform == "both":
            selected = ["linux", "macos"]
        elif platform in PLATFORMS:
            selected = [platform]
        else:
            raise CanaryError("manual platform is not supported")
    else:
        raise CanaryError("canary event is not schedule or workflow_dispatch")
    return {"include": [PLATFORMS[name] for name in selected]}


def schema_protocol(path: Path) -> int:
    try:
        protocol, _ = validate_schema(read_schema(path))
    except ContractError as error:
        raise CanaryError("Herdr schema failed the required API contract") from error
    return protocol


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("resolve", help="parse one bounded git ls-remote record")
    commands.add_parser("version", help="parse bounded `herdr --version` output")
    matrix = commands.add_parser("matrix", help="select the bounded runner matrix")
    matrix.add_argument("--event-name", required=True)
    matrix.add_argument("--schedule", default="")
    matrix.add_argument("--platform", default="")
    protocol = commands.add_parser("protocol", help="print a verified schema protocol")
    protocol.add_argument("schema", type=Path)
    args = parser.parse_args()

    try:
        if args.command == "resolve":
            print(resolve_master_sha(bounded_stdin()))
        elif args.command == "version":
            print(parse_version_output(bounded_stdin()))
        elif args.command == "matrix":
            print(
                json.dumps(
                    selected_matrix(args.event_name, args.schedule, args.platform),
                    separators=(",", ":"),
                    sort_keys=True,
                )
            )
        elif args.command == "protocol":
            print(schema_protocol(args.schema))
    except CanaryError as error:
        print(f"Herdr upstream canary error: {error}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
