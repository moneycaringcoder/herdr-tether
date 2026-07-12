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
OWNED_SESSION_RE = re.compile(r"^tether-[0-9a-f]{32}$")
SSH_TARGET_RE = re.compile(
    r"^(?:[A-Za-z0-9._-]+@)?[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?$"
)


def validate_remote_target(target: str) -> str:
    """Accept one explicit SSH destination, never options, URLs, or extra hosts."""
    if not SSH_TARGET_RE.fullmatch(target):
        raise ValueError(
            "remote target must be a single [user@]hostname or IPv4 address"
        )
    return target


class SmokeFailure(RuntimeError):
    pass


def fail(message: str) -> "NoReturn":
    raise SmokeFailure(message)


def format_command(argv: Iterable[str]) -> str:
    return " ".join(repr(arg) for arg in argv)

def terminal_screen_text(data: bytes, rows: int = 40, columns: int = 140) -> str:
    """Apply the small ANSI cursor/erase subset emitted by ratatui."""
    screen = [[" " for _ in range(columns)] for _ in range(rows)]
    row = 0
    column = 0
    text = data.decode("utf-8", "replace")
    index = 0
    while index < len(text):
        character = text[index]
        if character == "\x1b" and index + 1 < len(text):
            if text[index + 1] == "[":
                end = index + 2
                while end < len(text) and not ("@" <= text[end] <= "~"):
                    end += 1
                if end >= len(text):
                    break
                final = text[end]
                raw = text[index + 2 : end].lstrip("?")
                values = [
                    int(value) if value.isdigit() else 0
                    for value in raw.split(";")
                ] if raw else []
                first = values[0] if values else 0
                if final in ("H", "f"):
                    row = max(0, (values[0] if values and values[0] else 1) - 1)
                    column = max(
                        0, (values[1] if len(values) > 1 and values[1] else 1) - 1
                    )
                elif final == "A":
                    row = max(0, row - (first or 1))
                elif final == "B":
                    row = min(rows - 1, row + (first or 1))
                elif final == "C":
                    column = min(columns - 1, column + (first or 1))
                elif final == "D":
                    column = max(0, column - (first or 1))
                elif final == "G":
                    column = max(0, (first or 1) - 1)
                elif final == "d":
                    row = max(0, (first or 1) - 1)
                elif final == "J" and first in (2, 3):
                    screen = [[" " for _ in range(columns)] for _ in range(rows)]
                    row = 0
                    column = 0
                elif final == "K":
                    if first == 2:
                        screen[row] = [" " for _ in range(columns)]
                    elif first == 1:
                        for position in range(0, min(column + 1, columns)):
                            screen[row][position] = " "
                    else:
                        for position in range(column, columns):
                            screen[row][position] = " "
                index = end + 1
                continue
            if text[index + 1] == "]":
                end = text.find("\x07", index + 2)
                terminator = 1
                if end == -1:
                    end = text.find("\x1b\\", index + 2)
                    terminator = 2
                if end == -1:
                    break
                index = end + terminator
                continue
            index += 2
            continue
        if character == "\r":
            column = 0
        elif character == "\n":
            row = min(rows - 1, row + 1)
        elif character == "\b":
            column = max(0, column - 1)
        elif character >= " " and character != "\x7f":
            if row < rows and column < columns:
                screen[row][column] = character
            column = min(columns - 1, column + 1)
        index += 1
    return "\n".join("".join(line).rstrip() for line in screen)


class Smoke:
    def __init__(
        self,
        herdr: Path,
        tether: Path,
        repo_root: Path,
        keep: bool,
        remote_target: str | None = None,
        remote_directory: str | None = None,
        remote_known_hosts: Path | None = None,
    ) -> None:
        self.herdr = herdr.resolve()
        self.tether = tether.resolve()
        self.repo_root = repo_root.resolve()
        self.keep = keep
        self.remote_target = (
            validate_remote_target(remote_target) if remote_target is not None else None
        )
        self.remote_directory = remote_directory
        self.remote_known_hosts = (
            remote_known_hosts.resolve() if remote_known_hosts is not None else None
        )
        self.root = Path(tempfile.mkdtemp(prefix="tether-smoke-", dir="/tmp"))
        self.session = f"tether-smoke-{os.getpid()}"
        self.external_session = f"external-smoke-{os.getpid()}"
        self.herdr_client: subprocess.Popen[bytes] | None = None
        self.herdr_master: int | None = None
        self.herdr_output = bytearray()
        self._reader: threading.Thread | None = None
        self._cleaned = False
        self.owned_ids: set[str] = set()
        inherited_path = os.environ.get("PATH", "")
        resolved_tmux = shutil.which("tmux", path=inherited_path)
        self.tmux = Path(resolved_tmux).resolve() if resolved_tmux else Path("tmux")
        resolved_ssh = shutil.which("ssh", path=inherited_path)
        self.ssh = Path(resolved_ssh).resolve() if resolved_ssh else Path("ssh")
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
            self.root / "bin",
            home / ".ssh",
        ):
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        # macOS/Linux zsh otherwise blocks the first disposable shell on its
        # interactive new-user wizard before Herdr can run the pane command.
        (home / ".zshrc").write_text("# isolated live product smoke\n", encoding="utf-8")

        self.herdr_config = self.root / "config" / "herdr" / "config.toml"
        self.env = os.environ.copy()
        # Never inherit the caller's tmux socket or Herdr plugin/pane context:
        # every product and cleanup operation must stay under this smoke root.
        for variable in (
            "TMUX",
            "HERDR_BIN_PATH",
            "HERDR_PANE_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_PLUGIN_CONTEXT_JSON",
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
            "PANE_ID",
            "WORKSPACE_ID",
        ):
            self.env.pop(variable, None)
        original_home = Path(self.env.get("HOME", str(Path.home())))
        for variable, directory in (
            ("CARGO_HOME", original_home / ".cargo"),
            ("RUSTUP_HOME", original_home / ".rustup"),
        ):
            if variable not in self.env and directory.is_dir():
                self.env[variable] = str(directory)
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
        # Herdr's GUI plugin runtime does not inherit an interactive Homebrew
        # PATH. Product commands deliberately run under that same restriction;
        # Tether must resolve standard macOS package-manager locations itself.
        restricted_path = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        if self.remote_target is not None:
            if self.remote_known_hosts is None:
                fail("remote smoke requires an explicit --remote-known-hosts file")
            try:
                known_hosts = self.remote_known_hosts.read_bytes()
            except OSError as error:
                fail(f"could not read remote known-hosts file: {error}")
            if not known_hosts or len(known_hosts) > 1_048_576:
                fail("remote known-hosts file must contain between 1 byte and 1 MiB")
            isolated_known_hosts = home / ".ssh" / "known_hosts"
            isolated_known_hosts.write_bytes(known_hosts)
            isolated_known_hosts.chmod(0o600)
            ssh_config = home / ".ssh" / "config"
            ssh_config.write_text(
                "Host *\n"
                "    BatchMode yes\n"
                "    CanonicalizeHostname no\n"
                "    GlobalKnownHostsFile /dev/null\n"
                f"    UserKnownHostsFile {isolated_known_hosts}\n"
                "    StrictHostKeyChecking yes\n"
                "    ProxyCommand none\n"
                "    ProxyJump none\n",
                encoding="utf-8",
            )
            ssh_config.chmod(0o600)
            ssh_wrapper = self.root / "bin" / "ssh"
            ssh_wrapper.write_text(
                "#!/bin/sh\n"
                "allowed="
                + self._shell_quote(self.remote_target)
                + "\n"
                "found=false\n"
                "for arg do\n"
                '    if [ "$arg" = "$allowed" ]; then found=:; fi\n'
                "done\n"
                'if ! "$found"; then\n'
                '    echo "remote smoke refused unspecified SSH destination" >&2\n'
                "    exit 64\n"
                "fi\n"
                "exec "
                + self._shell_quote(str(self.ssh))
                + " -F "
                + self._shell_quote(str(ssh_config))
                + ' "$@"\n',
                encoding="utf-8",
            )
            ssh_wrapper.chmod(0o700)
            restricted_path.insert(0, str(self.root / "bin"))
        self.env["PATH"] = os.pathsep.join(restricted_path)
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
            except (OSError, ValueError, json.JSONDecodeError) as error:
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

        ready_since: float | None = None

        def ready() -> bool:
            nonlocal ready_since
            if self.herdr_client and self.herdr_client.poll() is not None:
                output = self.herdr_output.decode("utf-8", "replace")
                fail(f"Herdr PTY process exited early ({self.herdr_client.returncode})\n{output}")
            result = self.herdr_run("status", "server", check=False)
            if result.returncode != 0:
                ready_since = None
                return False
            if ready_since is None:
                ready_since = time.monotonic()
                return False
            return time.monotonic() - ready_since >= 0.5

        try:
            self.wait_until("Herdr server readiness", ready, START_TIMEOUT)
        except SmokeFailure as error:
            server_log = (
                self.herdr_config.parent
                / "sessions"
                / self.session
                / "herdr-server.log"
            )
            try:
                diagnostics = server_log.read_text(encoding="utf-8")
            except OSError as log_error:
                diagnostics = f"<unavailable: {log_error}>"
            fail(f"{error}\nHerdr server log ({server_log}):\n{diagnostics}")

    def interact(
        self,
        argv: list[str],
        env: dict[str, str],
        steps: list[tuple[str, bytes]],
    ) -> str:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        output = bytearray()
        try:
            process = subprocess.Popen(
                argv,
                cwd=self.repo_root,
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                close_fds=True,
            )
        except OSError as error:
            os.close(master)
            os.close(slave)
            fail(f"could not start interactive command {format_command(argv)}: {error}")
        os.close(slave)

        def drain() -> None:
            while True:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                output.extend(chunk)
                if b"\x1b[6n" in chunk:
                    os.write(master, b"\x1b[1;1R")

        reader = threading.Thread(target=drain, name="tether-picker-reader", daemon=True)
        reader.start()
        try:
            for marker, keys in steps:
                def visible() -> bool:
                    rendered = terminal_screen_text(bytes(output))
                    if process.poll() is not None and marker not in rendered:
                        fail(
                            f"interactive command exited ({process.returncode}) before marker "
                            f"{marker!r}: {format_command(argv)}\n{rendered}"
                        )
                    return marker in rendered

                try:
                    self.wait_until(f"interactive picker marker {marker!r}", visible)
                except SmokeFailure as error:
                    rendered = terminal_screen_text(bytes(output))
                    fail(
                        f"{error}; process={process.poll()}\n"
                        f"interactive screen:\n{rendered}"
                    )
                os.write(master, keys)
            try:
                returncode = process.wait(timeout=STATE_TIMEOUT)
            except subprocess.TimeoutExpired:
                process.terminate()
                fail(f"interactive command did not exit: {format_command(argv)}")
            rendered = output.decode("utf-8", "replace")
            if returncode != 0:
                fail(
                    f"interactive command failed ({returncode}): "
                    f"{format_command(argv)}\n{rendered[-8000:]}"
                )
            return rendered
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            try:
                os.close(master)
            except OSError:
                pass

    def picker_event_contract(self, workspace_id: str, invoking_pane: str) -> None:
        picker_env = self.tether_env(workspace_id, invoking_pane)
        before_panes = self.pane_ids()
        before_tmux = self.tmux_sessions()
        missing = self.root / "work" / "missing-create-directory"
        self.interact(
            [
                str(self.tether),
                "open",
                "--directory",
                str(missing),
                "--command",
                "true",
            ],
            picker_env,
            [
                ("Hosts", b"\r"),
                ("Create new Tether workload", b"\r"),
                (str(self.root / "home"), b"\r"),
                ("Shell", b"\r"),
                ("Split right", b"\x1b[B\x1b[B\r"),
                ("Operation failed", b""),
                ("Enter retry", b"\x1b"),
                ("Hosts", b"\x1b"),
            ],
        )
        if self.pane_ids() != before_panes or self.tmux_sessions() != before_tmux:
            fail("failed local new-tab create leaked a pane or tmux session")

        self.tmux_run(
            "new-session",
            "-d",
            "-s",
            self.external_session,
            "-c",
            str(self.root / "work"),
            "--",
            "/bin/sh",
            "-c",
            "exec sleep 300",
        )
        external_inventory = self.tmux_sessions()
        if self.external_session not in external_inventory:
            fail(
                "external fixture session exited immediately after creation; "
                f"isolated inventory: {sorted(external_inventory)}"
            )
        self.tmux_run(
            "set-option", "-t", self.external_session, "mouse", "off"
        )
        before_panes = self.pane_ids()
        self.interact(
            [str(self.tether), "open", "--host", "local"],
            picker_env,
            [
                ("Hosts", b"\r"),
                (self.external_session, b"\x1b[B\r"),
                ("Split right", b"\x1b[B\x1b[B\r"),
            ],
        )
        external_pane = self.wait_new_pane(before_panes, "external session new-tab pane")
        self.verify_placement("new-tab", invoking_pane, external_pane)
        mouse = self.tmux_value(
            "show-options", "-v", "-t", self.external_session, "mouse"
        )
        if mouse != "off":
            fail(f"external session mouse option was mutated to {mouse!r}")
        self.close_pane(external_pane)
        if self.external_session not in self.tmux_sessions():
            fail("closing an external Tether view killed the external tmux session")

    def replacement_contract(self, workspace_id: str, invoking_pane: str) -> None:
        split = self.result_object(
            self.decode_json(
                self.herdr_run(
                    "pane", "split", "--pane", invoking_pane, "--direction", "right", "--focus"
                ),
                "replacement source split",
            ),
            "replacement source split",
        )
        source_ids = self.collect_strings(split, "pane_id")
        if not source_ids:
            fail(f"Herdr omitted the replacement source identity: {split}")
        source = source_ids[-1]
        self.herdr_run("pane", "run", source, "exec sleep 300")
        def source_is_non_shell() -> bool:
            response = self.result_object(
                self.decode_json(
                    self.herdr_run("pane", "process-info", "--pane", source),
                    "replacement source process info",
                ),
                "replacement source process info",
            )
            names = self.collect_strings(response, "name")
            return "sleep" in names

        self.wait_until("replacement source foreground process", source_is_non_shell)
        source_env = self.tether_env(workspace_id, source)
        command = [
            str(self.tether),
            "open",
            "--host",
            "local",
            "--directory",
            str(self.root / "work"),
            "--command",
            "exec cat",
            "--placement",
            "replace-current-pane",
        ]

        before_ids = {item.get("id") for item in self.state_records()}
        before_tmux = self.tmux_sessions()
        refused = self.run(command, env=source_env, check=False)
        if refused.returncode == 0 or "source pane was preserved" not in refused.stderr:
            fail("noninteractive replacement did not preserve a non-shell source")
        if source not in self.pane_ids():
            fail("refused replacement closed its exact source pane")
        refused_ids = {
            item.get("id") for item in self.state_records()
        } - before_ids
        if len(refused_ids) != 1:
            fail(f"refused replacement did not retain one recoverable record: {refused_ids}")
        refused_id = next(iter(refused_ids))
        if not isinstance(refused_id, str) or not re.fullmatch(
            r"tether-[0-9a-f]{32}", refused_id
        ):
            fail(f"refused replacement produced a non-Tether ID: {refused_id!r}")
        if self.tmux_sessions() - before_tmux != {refused_id}:
            fail("refused replacement did not retain exactly its created workload")
        self.owned_ids.add(refused_id)
        self.run([str(self.tether), "session", "stop", refused_id])
        self.owned_ids.discard(refused_id)

        before_panes = self.pane_ids()
        before_ids = {item.get("id") for item in self.state_records()}
        self.interact(command, source_env, [("Continue? [y/N]", b"yes\r")])
        created_ids = {
            item.get("id") for item in self.state_records()
        } - before_ids
        if len(created_ids) != 1:
            fail(f"confirmed replacement did not create one exact record: {created_ids}")
        replacement_id = next(iter(created_ids))
        if not isinstance(replacement_id, str) or not re.fullmatch(
            r"tether-[0-9a-f]{32}", replacement_id
        ):
            fail(f"confirmed replacement produced a non-Tether ID: {replacement_id!r}")
        self.owned_ids.add(replacement_id)
        destination = self.wait_new_pane(before_panes - {source}, "replacement destination")
        if source in self.pane_ids():
            fail("confirmed replacement left the exact source pane open")
        self.verify_owned_tmux_contract(replacement_id, self.root / "work")
        self.close_pane(destination)
        self.run([str(self.tether), "session", "stop", replacement_id])
        self.owned_ids.discard(replacement_id)

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

    def keybinding_contract(self) -> None:
        original = self.herdr_config.read_bytes() if self.herdr_config.exists() else b""
        backup = self.herdr_config.with_name(
            self.herdr_config.name + ".tether-keybinding.bak"
        )
        key_env = self.env.copy()
        key_env["HERDR_BIN_PATH"] = str(self.herdr_wrapper)

        installed = self.run(
            [str(self.tether), "setup", "keybinding"], env=key_env
        )
        if "installed Herdr prefix+t" not in installed.stdout:
            fail(f"keybinding install did not report installation: {installed.stdout}")
        installed_bytes = self.herdr_config.read_bytes()
        if b'key = "prefix+t"' not in installed_bytes or PLUGIN_ID.encode() not in installed_bytes:
            fail("keybinding install omitted the exact prefix+t Tether plugin action")
        if backup.read_bytes() != original:
            fail("keybinding backup did not preserve the exact disposable Herdr config")

        fingerprint = self.file_fingerprint(self.herdr_config)
        idempotent = self.run(
            [str(self.tether), "setup", "keybinding"], env=key_env
        )
        if "already bound" not in idempotent.stdout:
            fail(f"idempotent keybinding install was not reported: {idempotent.stdout}")
        if self.file_fingerprint(self.herdr_config) != fingerprint:
            fail("idempotent keybinding install rewrote Herdr config")

        rolled_back = self.run(
            [str(self.tether), "setup", "keybinding", "--rollback"], env=key_env
        )
        if "restored Herdr config" not in rolled_back.stdout:
            fail(f"keybinding rollback was not reported: {rolled_back.stdout}")
        if self.herdr_config.read_bytes() != original:
            fail("keybinding rollback did not restore the exact Herdr config bytes")

        backup_fingerprint = self.file_fingerprint(backup)
        conflict = (
            original
            + (b"\n" if original and not original.endswith(b"\n") else b"")
            + b'[[keys.command]]\nkey = "prefix+t"\ntype = "command"\ncommand = "echo conflict"\n'
        )
        self.herdr_config.write_bytes(conflict)
        rejected = self.run(
            [str(self.tether), "setup", "keybinding"], env=key_env, check=False
        )
        if rejected.returncode == 0 or "already bound" not in rejected.stderr:
            fail(
                "conflicting prefix+t binding was not rejected with an actionable diagnostic"
            )
        if (
            self.herdr_config.read_bytes() != conflict
            or self.file_fingerprint(backup) != backup_fingerprint
        ):
            fail("conflicting keybinding attempt mutated config or backup")
        self.herdr_config.write_bytes(original)
        self.herdr_run("server", "reload-config")
        backup.unlink(missing_ok=True)


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
        workspace_id, invoking_pane = self.workspace_and_pane()
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
            if action == "open":
                pane_id = self.wait_new_pane(before, "open action managed pane")
                self.close_pane(pane_id)
            else:
                self.wait_new_pane(before, "setup action managed pane")
                plugin_config = (
                    self.root
                    / "config"
                    / "herdr"
                    / "plugins"
                    / "config"
                    / PLUGIN_ID
                    / "config.toml"
                )
                plugin_state = (
                    self.root
                    / "state"
                    / "herdr"
                    / "plugins"
                    / PLUGIN_ID
                    / "state.json"
                )
                self.wait_until(
                    "setup action plugin files",
                    lambda: plugin_config.is_file() and plugin_state.is_file(),
                )
                try:
                    json.loads(plugin_state.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError) as error:
                    fail(f"setup action state is unreadable or invalid JSON: {error}")
                plugin_env = self.tether_env(workspace_id, invoking_pane)
                plugin_env.update(
                    {
                        "HERDR_PLUGIN_CONFIG_DIR": str(plugin_config.parent),
                        "HERDR_PLUGIN_STATE_DIR": str(plugin_state.parent),
                    }
                )
                doctor = self.run(
                    [str(self.tether), "doctor"], env=plugin_env, check=False
                )
                if doctor.returncode == 0:
                    fail("restricted-path plugin doctor unexpectedly found Cargo")
                for executable in ("tmux", "ssh"):
                    if f"{executable}: ok" not in doctor.stdout:
                        fail(
                            f"plugin doctor did not resolve {executable} under the restricted GUI PATH: "
                            f"{doctor.stdout}"
                        )
                if "cargo: missing (install it or add it to PATH)" not in doctor.stdout:
                    fail(
                        "plugin doctor did not expose actionable missing-Cargo guidance: "
                        f"{doctor.stdout}"
                    )
                if self.herdr_master is None:
                    fail("Herdr PTY is unavailable while dismissing setup result")
                os.write(self.herdr_master, b"\r")
                self.wait_until(
                    "setup action managed pane exit",
                    lambda: not (self.pane_ids() - before),
                )
            logs = self.result_object(
                self.decode_json(
                    self.herdr_run("plugin", "log", "list", "--plugin", PLUGIN_ID, "--limit", "20"),
                    f"{action} action log",
                ),
                f"{action} action log",
            )
            if action not in json.dumps(logs, sort_keys=True):
                fail(f"Herdr plugin logs did not record the {action} action: {logs}")

        config_after = self.herdr_config.read_bytes()
        if b'key = "prefix+t"' not in config_after or PLUGIN_ID.encode() not in config_after:
            fail("explicit Tether launcher setup action did not install prefix+t")
        backup = self.herdr_config.with_name(
            self.herdr_config.name + ".tether-keybinding.bak"
        )
        if not backup.exists() or self.file_fingerprint(backup) != config_before:
            fail("explicit Tether launcher setup action did not preserve the exact config backup")

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

    def tmux_run(
        self, *args: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        return self.run([str(self.tmux), *args], check=check)

    def tmux_sessions(self) -> set[str]:
        result = self.tmux_run("list-sessions", "-F", "#{session_name}", check=False)
        if result.returncode not in (0, 1):
            fail(f"isolated tmux list failed: {result.stderr}")
        return {line for line in result.stdout.splitlines() if line}

    def tmux_attached(self, session_id: str) -> int:
        result = self.tmux_run(
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}",
            check=False,
        )
        if result.returncode == 1:
            return -1
        if result.returncode != 0:
            fail(f"isolated tmux attachment list failed: {result.stderr}")
        for line in result.stdout.splitlines():
            name, separator, attached = line.rpartition("\t")
            if separator and name == session_id:
                try:
                    return int(attached)
                except ValueError:
                    fail(
                        f"tmux returned an invalid attached count for "
                        f"{session_id}: {attached!r}"
                    )
        return -1

    def tmux_value(self, *args: str) -> str:
        result = self.tmux_run(*args)
        return result.stdout.rstrip("\r\n")

    def verify_owned_tmux_contract(self, session_id: str, directory: Path) -> None:
        pane_id = self.tmux_value("list-panes", "-t", f"={session_id}", "-F", "#{pane_id}")
        actual_cwd = self.tmux_value(
            "display-message", "-p", "-t", pane_id, "#{pane_current_path}"
        )
        try:
            same_directory = os.path.samefile(actual_cwd, directory)
        except OSError:
            same_directory = False
        if not same_directory:
            fail(
                f"owned session {session_id} cwd mismatch: "
                f"selected {directory}, got {actual_cwd!r}"
            )
        mouse = self.tmux_value("show-options", "-v", "-t", session_id, "mouse")
        if mouse != "on":
            fail(f"owned session {session_id} did not enable mouse; got {mouse!r}")

    def state_payload(self) -> dict[str, Any]:
        state_path = self.root / "state" / "herdr-tether" / "state.json"
        try:
            payload = json.loads(state_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            fail(f"could not read Tether state {state_path}: {error}")
        if not isinstance(payload, dict):
            fail(f"Tether state returned an unexpected shape: {payload}")
        return payload

    def state_records(self) -> list[dict[str, Any]]:
        payload = self.decode_json(
            self.run([str(self.tether), "session", "list", "--json"]),
            "Tether session list",
        )
        if not isinstance(payload, list) or not all(isinstance(item, dict) for item in payload):
            fail(f"Tether session list returned an unexpected shape: {payload}")
        return payload

    def create_placed_session(
        self,
        placement: str,
        workspace_id: str,
        invoking_pane: str,
        command: str,
        directory: Path | None = None,
    ) -> tuple[str, str, str]:
        directory = directory or self.root / "work"
        before_panes = self.pane_ids()
        before_tmux = self.tmux_sessions()
        result = self.run(
            [
                str(self.tether),
                "open",
                "--host",
                "local",
                "--directory",
                str(directory),
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
        self.owned_ids.add(session_id)
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
        self.verify_owned_tmux_contract(session_id, directory)
        if self.focused_pane() != pane_id:
            fail(
                f"Tether {placement} pane {pane_id} was not focused; "
                f"focused pane is {self.focused_pane()}"
            )
        self.verify_placement(placement, invoking_pane, pane_id)
        records = [item for item in self.state_records() if item.get("id") == session_id]
        if len(records) != 1 or str(records[0].get("status", "")).lower() != "running":
            fail(f"Tether did not persist one running record for {session_id}: {records}")
        return session_id, pane_id, str(tmux_created)

    def remote_cwd_contract(self, workspace_id: str, invoking_pane: str) -> None:
        if self.remote_target is None or self.remote_directory is None:
            return
        host_name = "tether-smoke-remote"
        self.run(
            [
                str(self.tether),
                "host",
                "add",
                host_name,
                self.remote_target,
                "--root",
                self.remote_directory,
            ]
        )
        before = {item.get("id") for item in self.state_records()}
        before_panes = self.pane_ids()
        created = self.run(
            [
                str(self.tether),
                "open",
                "--host",
                host_name,
                "--directory",
                self.remote_directory,
                "--command",
                "exec cat",
                "--placement",
                "new-tab",
            ],
            env=self.tether_env(workspace_id, invoking_pane),
        )
        match = re.search(r"^created (tether-[0-9a-f]{32})$", created.stdout, re.MULTILINE)
        if not match:
            fail(f"remote create did not report an exact Tether ID: {created.stdout}")
        session_id = match.group(1)
        if session_id in before:
            fail(f"remote create reused existing ID {session_id}")
        self.owned_ids.add(session_id)
        pane = self.wait_new_pane(before_panes, "remote new-tab pane")
        self.verify_placement("new-tab", invoking_pane, pane)

        def remote_tmux(*args: str) -> str:
            remote_command = " ".join(self._shell_quote(value) for value in ("tmux", *args))
            return self.run(
                [
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "--",
                    self.remote_target or "",
                    remote_command,
                ]
            ).stdout.rstrip("\r\n")

        actual_cwd = remote_tmux(
            "display-message", "-p", "-t", f"={session_id}:0.0", "#{pane_current_path}"
        )
        if actual_cwd != self.remote_directory:
            fail(
                f"remote session {session_id} cwd mismatch: expected "
                f"{self.remote_directory!r}, got {actual_cwd!r}"
            )
        if remote_tmux("show-options", "-v", "-t", session_id, "mouse") != "on":
            fail(f"remote owned session {session_id} did not enable mouse")
        self.close_pane(pane)
        self.run([str(self.tether), "session", "stop", session_id])
        self.owned_ids.discard(session_id)

    def observer_manager_contract(
        self,
        workspace_id: str,
        invoking_pane: str,
        owned_ids: set[str],
    ) -> None:
        picker_env = self.tether_env(workspace_id, invoking_pane)
        before_panes = self.pane_ids()
        before_tmux = self.tmux_sessions()
        before_status = {
            str(record.get("id")): str(record.get("status"))
            for record in self.state_records()
            if record.get("id") in owned_ids
        }

        self.interact(
            [str(self.tether), "open"],
            picker_env,
            [
                ("Hosts", b"o"),
                ("Tether \u00b7 Observers", b"n"),
                ("Choose orchestrator", b"\r"),
                ("Choose workers", b" \r"),
                ("Created Observer", b"\r"),
                ("Observer actions", b"\r"),
            ],
        )
        observer_pane = self.wait_new_pane(before_panes, "UI-first Observer companion")
        panes = self.pane_ids()
        if invoking_pane not in panes:
            fail("UI-first Observer launch closed its source launcher pane")
        if panes - before_panes != {observer_pane}:
            fail(
                "UI-first Observer launch did not create exactly one outer pane: "
                f"before={sorted(before_panes)}, after={sorted(panes)}"
            )
        if self.tmux_sessions() != before_tmux:
            fail("Observer metadata/create launch changed workload lifecycle")

        payload = self.state_payload()
        groups = payload.get("orchestration_groups") if isinstance(payload, dict) else None
        if not isinstance(groups, list) or len(groups) != 1:
            fail(f"UI-first Observer did not persist one group: {payload}")
        group = groups[0]
        workers = group.get("workers") if isinstance(group, dict) else None
        if not isinstance(workers, list) or len(workers) != 1:
            fail(f"UI-first Observer did not persist one selected worker: {group}")
        capabilities = workers[0].get("capabilities")
        if capabilities != {"observe_output": True, "open_interactive": True}:
            fail(f"UI-first Observer used unexpected capability defaults: {capabilities}")
        if group.get("orchestrator_session_id") == workers[0].get("session_id"):
            fail("UI-first Observer persisted its orchestrator as a worker")
        after_status = {
            str(record.get("id")): str(record.get("status"))
            for record in self.state_records()
            if record.get("id") in owned_ids
        }
        if after_status != before_status:
            fail(
                "UI-first Observer create/open changed workload state: "
                f"before={before_status}, after={after_status}"
            )

        self.close_pane(observer_pane)
        self.interact(
            [str(self.tether), "open"],
            picker_env,
            [
                ("Hosts", b"o"),
                ("Tether \u00b7 Observers", b"\r"),
                ("Observer actions", b"d"),
                ("Confirm delete", b"y"),
                ("Deleted Observer", b"\x1b"),
                ("Hosts", b"\x1b"),
            ],
        )
        payload = self.state_payload()
        groups = payload.get("orchestration_groups")
        if groups != []:
            fail(f"UI-first Observer deletion left group metadata: {groups}")
        final_status = {
            str(record.get("id")): str(record.get("status"))
            for record in self.state_records()
            if record.get("id") in owned_ids
        }
        if final_status != before_status or self.tmux_sessions() != before_tmux:
            fail("UI-first Observer deletion touched workload lifecycle")

    def product_lifecycle(self) -> None:
        setup = self.run([str(self.tether), "setup", "--yes"])
        if "Tether configuration:" not in setup.stdout or "Tether state:" not in setup.stdout:
            fail(f"Tether setup did not report its configuration and state paths: {setup.stdout}")
        doctor_env = self.env.copy()
        doctor_env["HERDR_BIN_PATH"] = str(self.herdr_wrapper)
        doctor = self.run([str(self.tether), "doctor"], env=doctor_env, check=False)
        if doctor.returncode == 0:
            fail("incomplete local Herdr context unexpectedly passed doctor")
        for executable in ("tmux", "ssh"):
            if f"{executable}: ok" not in doctor.stdout:
                fail(
                    f"doctor did not resolve {executable} under the restricted GUI PATH: "
                    f"{doctor.stdout}"
                )
        if "cargo: missing (install it or add it to PATH)" not in doctor.stdout:
            fail(
                "doctor did not expose actionable missing-tool guidance under "
                f"the restricted GUI PATH: {doctor.stdout}"
            )
        if "Herdr context: incomplete" not in doctor.stdout:
            fail(
                "standalone local doctor did not provide actionable incomplete "
                "plugin-context diagnostics"
            )

        workspace_id, initial_pane = self.workspace_and_pane()
        self.picker_event_contract(workspace_id, initial_pane)
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
        open_command = (
            self._shell_quote(str(self.tether)) + " session open " + self._shell_quote(first_id)
        )
        self.herdr_run("pane", "run", resumed_pane, open_command)
        self.wait_until(
            "resumed tmux attachment",
            lambda: self.tmux_attached(first_id) > 0,
        )
        if self.focused_pane() != resumed_pane:
            fail(f"resumed Tether view {resumed_pane} was not focused after attachment")
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

        symlink_directory = self.root / "selected-work-link"
        symlink_directory.symlink_to(self.root / "work", target_is_directory=True)
        symlink_id, symlink_pane, _ = self.create_placed_session(
            "split-right",
            workspace_id,
            initial_pane,
            "exec cat",
            directory=symlink_directory,
        )
        self.close_pane(symlink_pane)
        self.wait_until(
            "symlink-cwd tmux detach after Herdr view close",
            lambda: self.tmux_attached(symlink_id) == 0,
        )

        self.observer_manager_contract(
            workspace_id,
            initial_pane,
            {first_id, second_id, third_id, symlink_id},
        )

        for session_id in (first_id, second_id, third_id, symlink_id):
            self.run([str(self.tether), "session", "stop", session_id])
        self.wait_until(
            "all Tether tmux sessions to close",
            lambda: not ({first_id, second_id, third_id, symlink_id} & self.tmux_sessions()),
        )
        records = {item.get("id"): item for item in self.state_records()}
        for session_id in (first_id, second_id, third_id, symlink_id):
            status = str(records.get(session_id, {}).get("status", "")).lower()
            if status != "ended":
                fail(f"exact Stop did not persist ended status for {session_id}: {status!r}")
        self.replacement_contract(workspace_id, initial_pane)
        self.remote_cwd_contract(workspace_id, initial_pane)

    def validate_inputs(self) -> None:
        for label, path in (("Herdr", self.herdr), ("Tether", self.tether)):
            if not path.is_file() or not os.access(path, os.X_OK):
                fail(f"{label} executable is missing or not executable: {path}")
        if self.remote_directory is not None and not self.remote_directory.startswith("/"):
            fail("--remote-directory must be an absolute disposable POSIX path")
        if not (self.repo_root / "herdr-plugin.toml").is_file():
            fail(f"repository root has no herdr-plugin.toml: {self.repo_root}")
        if not self.tmux.is_file() or not os.access(self.tmux, os.X_OK):
            fail(
                "required executable tmux could not be resolved before entering "
                "the restricted GUI PATH"
            )
        tmux_version = self.run([str(self.tmux), "-V"]).stdout.strip()
        match = re.fullmatch(r"tmux (\d+)\.(\d+)[a-z]?", tmux_version)
        if not match or tuple(map(int, match.groups())) < (3, 3):
            fail(f"live smoke requires tmux 3.3 or newer; got {tmux_version!r}")
        version = self.run([str(self.herdr), "--version"])
        if version.stdout.strip() != f"herdr {HERDR_VERSION}":
            fail(
                f"live smoke requires official Herdr {HERDR_VERSION}; got {version.stdout.strip()!r}"
            )

    def cleanup(self) -> None:
        if self._cleaned:
            return
        self._cleaned = True
        # Close exact Tether-owned workloads first. The deliberately external
        # session is cleaned separately and is never passed to Tether close.
        for session_id in sorted(
            session_id
            for session_id in self.owned_ids
            if OWNED_SESSION_RE.fullmatch(session_id)
        ):
            try:
                self.run(
                    [str(self.tether), "session", "stop", session_id],
                    check=False,
                    timeout=5.0,
                )
            except SmokeFailure as error:
                print(f"live product smoke cleanup warning: {error}", file=sys.stderr)
        cleanup_commands = (
            ([str(self.tmux), "kill-session", "-t", f"={self.external_session}"], 5.0),
            ([str(self.herdr), "session", "stop", self.session, "--json"], 10.0),
            ([str(self.herdr), "session", "delete", self.session, "--json"], 10.0),
        )
        for command, timeout in cleanup_commands:
            try:
                self.run(command, check=False, timeout=timeout)
            except SmokeFailure as error:
                print(f"live product smoke cleanup warning: {error}", file=sys.stderr)
        if self.herdr_client and self.herdr_client.poll() is None:
            try:
                os.killpg(self.herdr_client.pid, signal.SIGTERM)
                self.herdr_client.wait(timeout=5.0)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                try:
                    os.killpg(self.herdr_client.pid, signal.SIGKILL)
                    self.herdr_client.wait(timeout=5.0)
                except (ProcessLookupError, subprocess.TimeoutExpired):
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
        self.keybinding_contract()
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
    parser.add_argument(
        "--remote-target",
        help="optional disposable SSH target for the remote cwd branch",
    )
    parser.add_argument(
        "--remote-directory",
        help="existing disposable absolute directory on --remote-target",
    )
    parser.add_argument(
        "--remote-known-hosts",
        type=Path,
        help="known_hosts file pinning the optional disposable SSH target",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    remote_values = (
        args.remote_target,
        args.remote_directory,
        args.remote_known_hosts,
    )
    if any(value is not None for value in remote_values) and not all(
        value is not None for value in remote_values
    ):
        print(
            "live product smoke FAILED: --remote-target, --remote-directory, and "
            "--remote-known-hosts must be supplied together",
            file=sys.stderr,
        )
        return 2
    if args.remote_target is not None:
        try:
            validate_remote_target(args.remote_target)
        except ValueError as error:
            print(f"live product smoke FAILED: {error}", file=sys.stderr)
            return 2
    smoke = Smoke(
        args.herdr,
        args.tether,
        args.repo_root,
        args.keep,
        args.remote_target,
        args.remote_directory,
        args.remote_known_hosts,
    )
    atexit.register(smoke.cleanup)

    def interrupted(signum: int, _frame: Any) -> None:
        fail(f"received signal {signum}")

    signal.signal(signal.SIGHUP, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    try:
        smoke.execute()
    except SmokeFailure as error:
        print(f"live product smoke FAILED: {error}", file=sys.stderr)
        output = smoke.herdr_output.decode("utf-8", "replace").strip()
        if output:
            print(f"Herdr PTY tail:\n{output[-8000:]}", file=sys.stderr)
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
