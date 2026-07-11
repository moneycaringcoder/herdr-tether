#!/usr/bin/env python3
"""Disposable live smoke for the Tether plugin, Herdr, and tmux.

This test intentionally uses only the Python standard library. Every command and
state path is rooted in a temporary directory, and every wait has a deadline.
"""

from __future__ import annotations

import argparse
import atexit
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from typing import Any, Callable, Iterable

PLUGIN_ID = "moneycaringcoder.tether"
HERDR_VERSION = "0.7.3"
COMMAND_TIMEOUT = 20.0
START_TIMEOUT = 30.0
STATE_TIMEOUT = 20.0


class SmokeFailure(RuntimeError):
    pass


def fail(message: str) -> "NoReturn":
    raise SmokeFailure(message)


def format_command(argv: Iterable[str]) -> str:
    return " ".join(repr(arg) for arg in argv)


class Smoke:
    def __init__(self, herdr: Path, tether: Path, repo_root: Path, keep: bool) -> None:
        self.herdr = herdr.resolve()
        self.tether = tether.resolve()
        self.repo_root = repo_root.resolve()
        self.keep = keep
        self.root = Path(tempfile.mkdtemp(prefix="tether-live-smoke-"))
        self.session = f"tether-smoke-{os.getpid()}"
        self.herdr_client: subprocess.Popen[bytes] | None = None
        self.herdr_master: int | None = None
        self.herdr_output = bytearray()
        self._reader: threading.Thread | None = None
        self._cleaned = False

        home = self.root / "home"
        for directory in (
            home,
            self.root / "config",
            self.root / "state",
            self.root / "data",
            self.root / "cache",
            self.root / "tmp",
            self.root / "tmux",
            self.root / "work",
        ):
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)

        self.herdr_config = self.root / "config" / "herdr" / "config.toml"
        self.env = os.environ.copy()
        # A smoke launched from inside tmux must never inherit its parent's
        # socket. TMUX overrides TMUX_TMPDIR and would make cleanup kill the
        # operator's server instead of this disposable one.
        self.env.pop("TMUX", None)
        self.env.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(self.root / "config"),
                "XDG_STATE_HOME": str(self.root / "state"),
                "XDG_DATA_HOME": str(self.root / "data"),
                "XDG_CACHE_HOME": str(self.root / "cache"),
                "TMPDIR": str(self.root / "tmp"),
                "TMUX_TMPDIR": str(self.root / "tmux"),
                "HERDR_CONFIG_PATH": str(self.herdr_config),
                "NO_COLOR": "1",
                "TERM": "xterm-256color",
            }
        )
        # A wrapper gives Tether's Herdr client the exact disposable named
        # session even outside a Herdr-launched plugin process.
        self.herdr_wrapper = self.root / "herdr-session"
        self.herdr_wrapper.write_text(
            "#!/bin/sh\nexec "
            + self._shell_quote(str(self.herdr))
            + " --session "
            + self._shell_quote(self.session)
            + " \"$@\"\n",
            encoding="utf-8",
        )
        self.herdr_wrapper.chmod(0o700)

    @staticmethod
    def _shell_quote(value: str) -> str:
        return "'" + value.replace("'", "'\\''") + "'"

    def run(
        self,
        argv: list[str],
        *,
        timeout: float = COMMAND_TIMEOUT,
        env: dict[str, str] | None = None,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        try:
            result = subprocess.run(
                argv,
                cwd=self.repo_root,
                env=env or self.env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            fail(f"command exceeded {timeout:.0f}s deadline: {format_command(argv)}")
        except OSError as error:
            fail(f"could not run {format_command(argv)}: {error}")
        if check and result.returncode != 0:
            fail(
                f"command failed ({result.returncode}): {format_command(argv)}\n"
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
            )
        return result

    def herdr_run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return self.run([str(self.herdr), "--session", self.session, *args], check=check)

    @staticmethod
    def decode_json(result: subprocess.CompletedProcess[str], operation: str) -> Any:
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError as error:
            fail(f"{operation} did not return JSON: {error}\nstdout:\n{result.stdout}")

    @staticmethod
    def result_object(payload: Any, operation: str) -> Any:
        if not isinstance(payload, dict):
            fail(f"{operation} returned a non-object JSON envelope")
        if "error" in payload:
            fail(f"{operation} returned an error: {payload['error']}")
        if "result" not in payload:
            fail(f"{operation} response omitted result: {payload}")
        return payload["result"]

    @staticmethod
    def collect_strings(value: Any, key: str) -> list[str]:
        found: list[str] = []
        if isinstance(value, dict):
            candidate = value.get(key)
            if isinstance(candidate, str) and candidate:
                found.append(candidate)
            for child in value.values():
                found.extend(Smoke.collect_strings(child, key))
        elif isinstance(value, list):
            for child in value:
                found.extend(Smoke.collect_strings(child, key))
        return found

    @staticmethod
    def find_objects(value: Any, key: str, expected: str) -> list[dict[str, Any]]:
        found: list[dict[str, Any]] = []
        if isinstance(value, dict):
            if value.get(key) == expected:
                found.append(value)
            for child in value.values():
                found.extend(Smoke.find_objects(child, key, expected))
        elif isinstance(value, list):
            for child in value:
                found.extend(Smoke.find_objects(child, key, expected))
        return found

    def wait_until(self, description: str, predicate: Callable[[], Any], timeout: float = STATE_TIMEOUT) -> Any:
        deadline = time.monotonic() + timeout
        last_error: Exception | None = None
        while time.monotonic() < deadline:
            try:
                value = predicate()
                if value:
                    return value
            except (OSError, ValueError, SmokeFailure, json.JSONDecodeError) as error:
                last_error = error
            remaining = deadline - time.monotonic()
            if remaining > 0:
                time.sleep(min(0.1, remaining))
        detail = f"; last error: {last_error}" if last_error else ""
        fail(f"deadline exceeded waiting for {description}{detail}")

    def start_herdr(self) -> None:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        try:
            self.herdr_client = subprocess.Popen(
                [str(self.herdr), "--session", self.session],
                cwd=self.repo_root,
                env=self.env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                start_new_session=True,
                close_fds=True,
            )
        except OSError as error:
            os.close(master)
            os.close(slave)
            fail(f"could not start Herdr under a PTY: {error}")
        finally:
            try:
                os.close(slave)
            except OSError:
                pass
        self.herdr_master = master

        def drain() -> None:
            while True:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                self.herdr_output.extend(chunk)
                if len(self.herdr_output) > 1_000_000:
                    del self.herdr_output[:-500_000]

        self._reader = threading.Thread(target=drain, name="herdr-pty-reader", daemon=True)
        self._reader.start()

        def ready() -> bool:
            if self.herdr_client and self.herdr_client.poll() is not None:
                output = self.herdr_output.decode("utf-8", "replace")
                fail(f"Herdr PTY process exited early ({self.herdr_client.returncode})\n{output}")
            result = self.herdr_run("status", "server", check=False)
            return result.returncode == 0

        self.wait_until("Herdr server readiness", ready, START_TIMEOUT)

    def workspace_and_pane(self) -> tuple[str, str]:
        workspaces = self.result_object(
            self.decode_json(self.herdr_run("workspace", "list"), "workspace list"),
            "workspace list",
        )
        workspace_ids = self.collect_strings(workspaces, "workspace_id")
        if not workspace_ids:
            fail(f"Herdr workspace list reported no workspace identity: {workspaces}")
        panes = self.pane_ids()
        if not panes:
            fail("Herdr pane list reported no pane identity")
        return workspace_ids[0], sorted(panes)[0]

    def pane_ids(self) -> set[str]:
        payload = self.result_object(
            self.decode_json(self.herdr_run("pane", "list"), "pane list"), "pane list"
        )
        return set(self.collect_strings(payload, "pane_id"))

    def pane_tab_id(self, pane_id: str) -> str:
        payload = self.result_object(
            self.decode_json(self.herdr_run("pane", "list"), "pane list"), "pane list"
        )
        panes = self.find_objects(payload, "pane_id", pane_id)
        tab_ids = {
            pane.get("tab_id")
            for pane in panes
            if isinstance(pane.get("tab_id"), str) and pane.get("tab_id")
        }
        if len(tab_ids) != 1:
            fail(f"Herdr pane {pane_id} did not map to exactly one tab: {panes}")
        return next(iter(tab_ids))

    def verify_placement(self, placement: str, invoking_pane: str, pane_id: str) -> None:
        invoking_tab = self.pane_tab_id(invoking_pane)
        created_tab = self.pane_tab_id(pane_id)
        if placement == "new-tab":
            if created_tab == invoking_tab:
                fail(f"new-tab placement reused invoking tab {invoking_tab}")
            return
        if created_tab != invoking_tab:
            fail(
                f"{placement} pane {pane_id} moved to tab {created_tab}; "
                f"expected invoking tab {invoking_tab}"
            )
        expected_direction = {
            "split-right": "right",
            "split-down": "down",
        }[placement]
        layout = self.result_object(
            self.decode_json(
                self.herdr_run("pane", "layout", "--pane", pane_id),
                f"{placement} pane layout",
            ),
            f"{placement} pane layout",
        )
        directions = set(self.collect_strings(layout, "direction"))
        if expected_direction not in directions:
            fail(
                f"{placement} layout omitted {expected_direction!r} split: {layout}"
            )
        zoomed = [
            value.get("zoomed")
            for value in self.find_objects(layout, "focused_pane_id", pane_id)
            if "zoomed" in value
        ]
        if zoomed and any(value is not False for value in zoomed):
            fail(f"{placement} layout remained zoomed: {layout}")

    def focused_pane(self) -> str:
        payload = self.result_object(
            self.decode_json(self.herdr_run("pane", "current"), "pane current"),
            "pane current",
        )
        pane_ids = self.collect_strings(payload, "pane_id")
        if len(set(pane_ids)) != 1:
            fail(f"Herdr pane current did not identify exactly one pane: {payload}")
        return pane_ids[0]

    def wait_new_pane(self, before: set[str], description: str) -> str:
        def appeared() -> str | bool:
            created = self.pane_ids() - before
            if len(created) > 1:
                fail(f"{description} created multiple panes unexpectedly: {sorted(created)}")
            return next(iter(created)) if created else False

        return self.wait_until(description, appeared)

    def close_pane(self, pane_id: str) -> None:
        self.herdr_run("plugin", "pane", "close", pane_id, check=False)
        self.herdr_run("pane", "close", pane_id, check=False)
        self.wait_until(f"pane {pane_id} closure", lambda: pane_id not in self.pane_ids())

    def plugin_contract(self) -> None:
        linked = self.result_object(
            self.decode_json(
                self.herdr_run("plugin", "link", str(self.repo_root)), "plugin link"
            ),
            "plugin link",
        )
        if PLUGIN_ID not in json.dumps(linked, sort_keys=True):
            fail(f"plugin link response did not name {PLUGIN_ID}: {linked}")

        actions = self.result_object(
            self.decode_json(
                self.herdr_run("plugin", "action", "list", "--plugin", PLUGIN_ID),
                "plugin action list",
            ),
            "plugin action list",
        )
        action_ids = set(self.collect_strings(actions, "action_id")) | set(
            self.collect_strings(actions, "id")
        )
        rendered = json.dumps(actions, sort_keys=True)
        for action in ("open", "setup"):
            if action not in action_ids and f"{PLUGIN_ID}.{action}" not in rendered:
                fail(f"plugin action listing omitted {action!r}: {actions}")

        config_before = self.file_fingerprint(self.herdr_config)
        for action in ("open", "setup"):
            before = self.pane_ids()
            response = self.result_object(
                self.decode_json(
                    self.herdr_run(
                        "plugin", "action", "invoke", action, "--plugin", PLUGIN_ID
                    ),
                    f"invoke {action} action",
                ),
                f"invoke {action} action",
            )
            if "started" not in json.dumps(response, sort_keys=True).lower():
                fail(f"{action} action did not report a started command: {response}")
            pane_id = self.wait_new_pane(before, f"{action} action managed pane")
            self.close_pane(pane_id)
            logs = self.result_object(
                self.decode_json(
                    self.herdr_run("plugin", "log", "list", "--plugin", PLUGIN_ID, "--limit", "20"),
                    f"{action} action log",
                ),
                f"{action} action log",
            )
            if action not in json.dumps(logs, sort_keys=True):
                fail(f"Herdr plugin logs did not record the {action} action: {logs}")

        config_after = self.file_fingerprint(self.herdr_config)
        if config_after != config_before:
            fail("Tether setup action altered Herdr's config file")

    @staticmethod
    def file_fingerprint(path: Path) -> tuple[bool, str]:
        if not path.exists():
            return False, ""
        if not path.is_file():
            fail(f"expected regular config file at {path}")
        return True, hashlib.sha256(path.read_bytes()).hexdigest()

    def tether_env(self, workspace_id: str, pane_id: str) -> dict[str, str]:
        env = self.env.copy()
        env.update(
            {
                "HERDR_BIN_PATH": str(self.herdr_wrapper),
                "HERDR_WORKSPACE_ID": workspace_id,
                "HERDR_PANE_ID": pane_id,
                "HERDR_PLUGIN_CONTEXT_JSON": json.dumps(
                    {"focused_pane_id": pane_id}, separators=(",", ":")
                ),
            }
        )
        return env

    def tmux_sessions(self) -> set[str]:
        result = self.run(
            ["tmux", "list-sessions", "-F", "#{session_name}"], check=False
        )
        if result.returncode not in (0, 1):
            fail(f"isolated tmux list failed: {result.stderr}")
        return {line for line in result.stdout.splitlines() if line}

    def tmux_attached(self, session_id: str) -> int:
        result = self.run(
            [
                "tmux",
                "display-message",
                "-p",
                "-t",
                f"={session_id}",
                "#{session_attached}",
            ],
            check=False,
        )
        if result.returncode != 0:
            return -1
        try:
            return int(result.stdout.strip())
        except ValueError:
            fail(f"tmux returned an invalid attached count for {session_id}: {result.stdout!r}")

    def state_records(self) -> list[dict[str, Any]]:
        payload = self.decode_json(
            self.run([str(self.tether), "session", "list", "--json"]),
            "Tether session list",
        )
        if not isinstance(payload, list) or not all(isinstance(item, dict) for item in payload):
            fail(f"Tether session list returned an unexpected shape: {payload}")
        return payload

    def create_placed_session(
        self, placement: str, workspace_id: str, invoking_pane: str, command: str
    ) -> tuple[str, str, str]:
        before_panes = self.pane_ids()
        before_tmux = self.tmux_sessions()
        result = self.run(
            [
                str(self.tether),
                "open",
                "--host",
                "local",
                "--directory",
                str(self.root / "work"),
                "--command",
                command,
                "--placement",
                placement,
            ],
            env=self.tether_env(workspace_id, invoking_pane),
        )
        match = re.search(r"^created (tether-[0-9a-f]{32})$", result.stdout, re.MULTILINE)
        if not match:
            fail(f"Tether {placement} create did not print a session ID: {result.stdout}")
        session_id = match.group(1)
        pane_id = self.wait_new_pane(before_panes, f"Tether {placement} pane")
        tmux_created = self.wait_until(
            f"Tether {placement} tmux session",
            lambda: next(iter(self.tmux_sessions() - before_tmux), False),
        )
        if tmux_created != session_id:
            fail(
                f"Tether {placement} record {session_id} did not match tmux {tmux_created}"
            )
        self.wait_until(
            f"Tether {placement} tmux attachment",
            lambda: self.tmux_attached(session_id) > 0,
        )
        if self.focused_pane() != pane_id:
            fail(
                f"Tether {placement} pane {pane_id} was not focused; "
                f"focused pane is {self.focused_pane()}"
            )
        self.verify_placement(placement, invoking_pane, pane_id)
        records = [item for item in self.state_records() if item.get("id") == session_id]
        if len(records) != 1 or str(records[0].get("status", "")).lower() != "active":
            fail(f"Tether did not persist one active record for {session_id}: {records}")
        return session_id, pane_id, str(tmux_created)

    def product_lifecycle(self) -> None:
        setup = self.run([str(self.tether), "setup", "--yes"])
        if "Tether configuration:" not in setup.stdout or "Tether state:" not in setup.stdout:
            fail(f"Tether setup did not report its configuration and state paths: {setup.stdout}")

        workspace_id, initial_pane = self.workspace_and_pane()
        pid_file = self.root / "work" / "continuity.pid"
        workload = (
            f"exec {self._shell_quote(sys.executable)} -c "
            + self._shell_quote(
                "import os,pathlib,signal;"
                f"pathlib.Path({str(pid_file)!r}).write_text(str(os.getpid()));"
                "signal.pause()"
            )
        )

        first_id, first_pane, first_tmux = self.create_placed_session(
            "split-right", workspace_id, initial_pane, workload
        )
        self.wait_until("continuity workload PID file", pid_file.exists)
        original_pid = pid_file.read_text(encoding="utf-8").strip()
        if not original_pid.isdigit():
            fail(f"continuity workload wrote invalid PID {original_pid!r}")

        # Closing the Herdr view must not close Tether's tmux workload.
        self.close_pane(first_pane)
        self.wait_until(
            "durable tmux detach after Herdr view close",
            lambda: self.tmux_attached(first_id) == 0,
        )
        if first_tmux not in self.tmux_sessions():
            fail("closing the Herdr pane killed the durable tmux session")
        if pid_file.read_text(encoding="utf-8").strip() != original_pid:
            fail("workload PID changed after closing its Herdr view")

        # Resume the exact session in a new, actual Herdr pane and retain PID.
        split = self.result_object(
            self.decode_json(
                self.herdr_run(
                    "pane", "split", "--pane", initial_pane, "--direction", "right", "--focus"
                ),
                "resume pane split",
            ),
            "resume pane split",
        )
        resumed_ids = self.collect_strings(split, "pane_id")
        if not resumed_ids:
            fail(f"Herdr did not return the resumed pane identity: {split}")
        resumed_pane = resumed_ids[-1]
        resume_command = (
            self._shell_quote(str(self.tether)) + " session resume " + self._shell_quote(first_id)
        )
        self.herdr_run("pane", "run", resumed_pane, resume_command)
        self.wait_until("resumed Tether view focus", lambda: self.focused_pane() == resumed_pane)
        self.wait_until(
            "resumed tmux attachment",
            lambda: self.tmux_attached(first_id) > 0,
        )
        if pid_file.read_text(encoding="utf-8").strip() != original_pid:
            fail("resuming the Tether session replaced the workload process")
        self.close_pane(resumed_pane)
        self.wait_until(
            "resumed tmux detach after Herdr view close",
            lambda: self.tmux_attached(first_id) == 0,
        )

        # Exercise the other placements through Tether itself. Each result is
        # correlated with the actual pane identity returned by the live server.
        second_id, second_pane, _ = self.create_placed_session(
            "split-down", workspace_id, initial_pane, "exec cat"
        )
        self.close_pane(second_pane)
        self.wait_until(
            "split-down tmux detach after Herdr view close",
            lambda: self.tmux_attached(second_id) == 0,
        )
        third_id, third_pane, _ = self.create_placed_session(
            "new-tab", workspace_id, initial_pane, "exec cat"
        )
        self.close_pane(third_pane)
        self.wait_until(
            "new-tab tmux detach after Herdr view close",
            lambda: self.tmux_attached(third_id) == 0,
        )

        for session_id in (first_id, second_id, third_id):
            self.run([str(self.tether), "session", "close", session_id])
        self.wait_until("all Tether tmux sessions to close", lambda: not self.tmux_sessions())
        records = {item.get("id"): item for item in self.state_records()}
        for session_id in (first_id, second_id, third_id):
            status = str(records.get(session_id, {}).get("status", "")).lower()
            if status != "closed":
                fail(f"exact close did not persist closed status for {session_id}: {status!r}")

    def validate_inputs(self) -> None:
        for label, path in (("Herdr", self.herdr), ("Tether", self.tether)):
            if not path.is_file() or not os.access(path, os.X_OK):
                fail(f"{label} executable is missing or not executable: {path}")
        if not (self.repo_root / "herdr-plugin.toml").is_file():
            fail(f"repository root has no herdr-plugin.toml: {self.repo_root}")
        for command in ("tmux",):
            if shutil.which(command, path=self.env.get("PATH")) is None:
                fail(f"required executable is not on PATH: {command}")
        version = self.run([str(self.herdr), "--version"])
        if version.stdout.strip() != f"herdr {HERDR_VERSION}":
            fail(
                f"live smoke requires official Herdr {HERDR_VERSION}; got {version.stdout.strip()!r}"
            )

    def cleanup(self) -> None:
        if self._cleaned:
            return
        self._cleaned = True
        # Close product workloads first, then both persistent multiplexers.
        self.run(["tmux", "kill-server"], check=False, timeout=5.0)
        self.run(
            [str(self.herdr), "session", "stop", self.session, "--json"],
            check=False,
            timeout=10.0,
        )
        self.run(
            [str(self.herdr), "session", "delete", self.session, "--json"],
            check=False,
            timeout=10.0,
        )
        if self.herdr_client and self.herdr_client.poll() is None:
            try:
                os.killpg(self.herdr_client.pid, signal.SIGTERM)
                self.herdr_client.wait(timeout=5.0)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                try:
                    os.killpg(self.herdr_client.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        if self.herdr_master is not None:
            try:
                os.close(self.herdr_master)
            except OSError:
                pass
            self.herdr_master = None
        if not self.keep:
            shutil.rmtree(self.root, ignore_errors=True)

    def execute(self) -> None:
        self.validate_inputs()
        self.start_herdr()
        self.plugin_contract()
        self.product_lifecycle()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Tether against live Herdr 0.7.3 and an isolated tmux server."
    )
    parser.add_argument("--herdr", required=True, type=Path, help="official Herdr 0.7.3 binary")
    parser.add_argument("--tether", required=True, type=Path, help="built herdr-tether binary")
    parser.add_argument(
        "--repo-root", type=Path, default=Path.cwd(), help="checkout containing herdr-plugin.toml"
    )
    parser.add_argument("--keep", action="store_true", help="retain the disposable root for debugging")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    smoke = Smoke(args.herdr, args.tether, args.repo_root, args.keep)
    atexit.register(smoke.cleanup)
    try:
        smoke.execute()
    except SmokeFailure as error:
        print(f"live product smoke FAILED: {error}", file=sys.stderr)
        if args.keep:
            print(f"disposable root retained at {smoke.root}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("live product smoke interrupted", file=sys.stderr)
        return 130
    finally:
        smoke.cleanup()
    print("live product smoke passed: actions, continuity, exact close, and all placements")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
