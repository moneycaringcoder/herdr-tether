#!/usr/bin/env python3
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import signal
import tempfile
import unittest
from unittest import mock

import live_product_smoke as smoke_module
from live_product_smoke import (
    Smoke,
    SmokeInterrupted,
    REPORT_MAX_BYTES,
    failure_category,
    finalize_cleanup_verdict,
    smoke_report,
    process_fingerprint,
    terminal_screen_text,
    validate_remote_target,
)


class SmokeJsonReportTests(unittest.TestCase):
    def test_report_is_exact_bounded_and_redacts_adversarial_failures(self) -> None:
        error = RuntimeError(
            "command failed credential-token /private/home/repository "
            "host.internal session-id \x1b]0;owned\x07 " + "x" * 100_000
        )
        smoke = mock.Mock(
            completed_phases=["validate_inputs", "start_herdr"],
            active_phase="keybinding_contract",
            cleanup_attempts=4,
            cleanup_result="failed",
            tmux_version="tmux 3.4",
            tether_version="herdr-tether 0.1.0",
        )
        report = smoke_report(smoke, "failed", failure_category(error))
        encoded = json.dumps(report, separators=(",", ":"))
        self.assertLessEqual(len(encoded.encode()), REPORT_MAX_BYTES)
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual(report["completion"], "failed")
        self.assertEqual(report["failure_category"], "command")
        self.assertEqual(
            [phase["status"] for phase in report["phases"]],
            ["passed", "passed", "failed", "not_run", "not_run", "not_run"],
        )
        self.assertEqual(report["exercised"], {"actions": [], "placements": []})
        self.assertEqual(report["cleanup"], {"attempts": 4, "result": "failed"})
        self.assertFalse(report["truncated"])
        for forbidden in (
            "credential-token",
            "/private/home",
            "host.internal",
            "session-id",
            "\x1b",
            "x" * 100,
        ):
            self.assertNotIn(forbidden, encoded)

    def test_success_report_has_stable_actions_placements_and_versions(self) -> None:
        smoke = mock.Mock(
            completed_phases=[
                "validate_inputs",
                "start_herdr",
                "keybinding_contract",
                "plugin_contract",
                "keyboard_picker_matrix",
                "product_lifecycle",
            ],
            active_phase=None,
            cleanup_attempts=3,
            cleanup_result="passed",
            tmux_version="tmux 3.4",
            tether_version="herdr-tether 0.1.0",
        )
        report = smoke_report(smoke, "passed")
        self.assertEqual(report["completion"], "complete")
        self.assertEqual(
            report["exercised"]["placements"],
            ["split-right", "split-down", "new-tab"],
        )
        self.assertEqual(
            report["exercised"]["actions"],
            ["setup", "doctor", "open", "resume", "stop", "replace", "observe"],
        )
        self.assertIsNone(report["failure_category"])


class SmokeEnvironmentTests(unittest.TestCase):
    def test_parent_tmux_socket_is_not_inherited(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            with mock.patch.dict(
                os.environ,
                {
                    "TMUX": "/tmp/tmux-parent/default,123,0",
                    "HERDR_BIN_PATH": "/operator/herdr",
                    "HERDR_PANE_ID": "operator-pane",
                    "HERDR_WORKSPACE_ID": "operator-workspace",
                    "HERDR_PLUGIN_CONTEXT_JSON": "{}",
                    "HERDR_PLUGIN_CONFIG_DIR": "/operator/config",
                    "HERDR_PLUGIN_STATE_DIR": "/operator/state",
                    "PANE_ID": "legacy-pane",
                    "WORKSPACE_ID": "legacy-workspace",
                },
                clear=False,
            ):
                smoke = Smoke(
                    Path("/bin/true"),
                    Path("/bin/true"),
                    Path(repository),
                    keep=False,
                )
            try:
                self.assertNotIn("TMUX", smoke.env)
                for variable in (
                    "HERDR_BIN_PATH",
                    "HERDR_PANE_ID",
                    "HERDR_WORKSPACE_ID",
                    "HERDR_PLUGIN_CONTEXT_JSON",
                    "HERDR_PLUGIN_CONFIG_DIR",
                    "HERDR_PLUGIN_STATE_DIR",
                    "PANE_ID",
                    "WORKSPACE_ID",
                ):
                    self.assertNotIn(variable, smoke.env)
                tmux_root = Path(smoke.env["TMUX_TMPDIR"])
                self.assertTrue(tmux_root.is_relative_to(smoke.root))
                self.assertTrue(tmux_root.is_dir())
                client_socket = (
                    smoke.root
                    / "config"
                    / "herdr"
                    / "sessions"
                    / smoke.session
                    / "herdr-client.sock"
                )
                self.assertEqual(smoke.root.parent, Path("/tmp"))
                self.assertLess(len(os.fsencode(client_socket)), 104)
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)
    def test_gui_runtime_path_is_restricted_after_tmux_is_resolved(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=False,
            )
            try:
                self.assertEqual(
                    smoke.env["PATH"], "/usr/bin:/bin:/usr/sbin:/sbin"
                )
                self.assertNotIn("homebrew", smoke.env["PATH"].lower())
                self.assertNotIn("/usr/local/bin", smoke.env["PATH"])
                self.assertTrue(smoke.tmux.is_absolute())
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)

    def test_remote_target_is_a_single_explicit_ssh_destination(self) -> None:
        for target in ("host.example", "runner@host.example", "runner@192.0.2.10"):
            self.assertEqual(validate_remote_target(target), target)
        for target in (
            "",
            "-oProxyCommand=evil",
            "host.example other.example",
            "ssh://host.example",
            "runner@host.example:22",
            "runner@@host.example",
        ):
            with self.subTest(target=target):
                with self.assertRaises(ValueError):
                    validate_remote_target(target)

    def test_remote_known_hosts_is_copied_into_isolated_home(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            source = Path(repository) / "known_hosts"
            contents = "host.example ssh-ed25519 AAAAC3NzaFixtureOnly\n"
            source.write_text(contents, encoding="utf-8")
            source.chmod(0o644)
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=False,
                remote_target="runner@host.example",
                remote_directory="/srv/tether-smoke",
                remote_known_hosts=source,
            )
            try:
                isolated = Path(smoke.env["HOME"]) / ".ssh" / "known_hosts"
                self.assertEqual(isolated.read_text(encoding="utf-8"), contents)
                self.assertEqual(isolated.stat().st_mode & 0o777, 0o600)
                refused = subprocess.run(
                    [str(smoke.root / "bin" / "ssh"), "other.example", "true"],
                    check=False,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=5,
                )
                self.assertEqual(refused.returncode, 64)
                self.assertIn("refused unspecified SSH destination", refused.stderr)
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)

    def test_owned_tmux_contract_queries_exact_id_without_touching_external(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=False,
            )
            try:
                directory = smoke.root / "work"
                with mock.patch.object(
                    smoke,
                    "tmux_value",
                    side_effect=["%42", str(directory), "on"],
                ) as tmux_value:
                    smoke.verify_owned_tmux_contract(
                        "tether-0123456789abcdef0123456789abcdef", directory
                    )
                self.assertEqual(
                    tmux_value.call_args_list,
                    [
                        mock.call(
                            "list-panes",
                            "-t",
                            "=tether-0123456789abcdef0123456789abcdef",
                            "-F",
                            "#{pane_id}",
                        ),
                        mock.call(
                            "display-message",
                            "-p",
                            "-t",
                            "%42",
                            "#{pane_current_path}",
                        ),
                        mock.call(
                            "show-options",
                            "-v",
                            "-t",
                            "tether-0123456789abcdef0123456789abcdef",
                            "mouse",
                        ),
                    ],
                )
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)
    def test_cleanup_targets_only_exact_owned_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=True,
            )
            valid_id = "tether-0123456789abcdef0123456789abcdef"
            smoke.owned_ids.update(
                {
                    valid_id,
                    "not-a-tether-session",
                    "tether-0123456789abcdef0123456789abcdef:other",
                }
            )
            try:
                completed = mock.Mock(returncode=0, stdout="", stderr="")
                with mock.patch.object(
                    smoke, "run", return_value=completed
                ) as run:
                    smoke.cleanup()

                commands = [call.args[0] for call in run.call_args_list]
                tether_stop_commands = [
                    command
                    for command in commands
                    if command[:3] == [str(smoke.tether), "session", "stop"]
                    and len(command) == 4
                ]
                self.assertEqual(
                    tether_stop_commands,
                    [[str(smoke.tether), "session", "stop", valid_id]],
                )
                self.assertFalse(
                    any(
                        command[1:] == ["kill-server"]
                        for command in commands
                    ),
                    commands,
                )
                kill_targets = [
                    command
                    for command in commands
                    if len(command) > 1 and command[1] == "kill-session"
                ]
                self.assertEqual(
                    kill_targets,
                    [
                        [
                            str(smoke.tmux),
                            "kill-session",
                            "-t",
                            f"={smoke.external_session}",
                        ]
                    ],
                )
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)




    def test_process_fingerprint_extracts_nested_identity_deterministically(self) -> None:
        payload = {
            "result": {
                "children": [
                    {"pid": 42, "name": "sh", "argv": ["sh", "-c", "exec worker"]},
                    {
                        "nested": {
                            "pid": 43,
                            "name": "herdr-tether",
                            "argv": ["herdr-tether", "observer-runtime"],
                        }
                    },
                    {"pid": 42, "name": "sh", "argv": ["sh", "-c", "exec worker"]},
                ]
            }
        }

        self.assertEqual(
            process_fingerprint(payload),
            (
                ("42", "sh", ("sh", "-c", "exec worker")),
                ("43", "herdr-tether", ("herdr-tether", "observer-runtime")),
            ),
        )

    def test_terminal_screen_tracks_ratatui_cursor_updates(self) -> None:
        output = (
            b"\x1b[2J\x1b[1;1H"
            b"Tether - Hosts"
            b"\x1b[1;10HResources"
            b"\x1b[2;1HCreate new Tether workload"
            b"\x1b[3;1H\x1b[31mSplit right\x1b[0m"
        )
        screen = terminal_screen_text(output, rows=4, columns=60)
        self.assertIn("Tether - Resources", screen)
        self.assertNotIn("Tether - Hosts", screen)
        self.assertIn("Create new Tether workload", screen)
        self.assertIn("Split right", screen)

    def test_herdr_resize_uses_explicit_rows_and_columns(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=False,
            )
            smoke.herdr_master = 17
            try:
                with mock.patch.object(smoke_module.fcntl, "ioctl") as ioctl:
                    smoke.resize_herdr(rows=14, columns=48)
                descriptor, operation, dimensions = ioctl.call_args.args
                self.assertEqual(descriptor, 17)
                self.assertEqual(operation, smoke_module.termios.TIOCSWINSZ)
                self.assertEqual(
                    smoke_module.struct.unpack("HHHH", dimensions),
                    (14, 48, 0, 0),
                )
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)

    def test_keyboard_picker_matrix_orchestrates_semantic_stages_without_sleep(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=False,
            )
            panes = iter(["picker-wide", "picker-narrow"])
            try:
                with (
                    mock.patch.object(smoke, "resize_herdr") as resize,
                    mock.patch.object(
                        smoke,
                        "invoke_plugin_picker_via_keyboard",
                        side_effect=lambda: next(panes),
                    ) as invoke,
                    mock.patch.object(
                        smoke, "interact_managed_pane_via_herdr"
                    ) as interact,
                    mock.patch.object(
                        smoke, "wait_until", return_value=True
                    ) as wait_until,
                    mock.patch.object(smoke_module.time, "sleep") as sleep,
                ):
                    smoke.keyboard_picker_matrix()

                self.assertEqual(
                    resize.call_args_list,
                    [
                        mock.call(rows=24, columns=80),
                        mock.call(rows=40, columns=140),
                        mock.call(rows=14, columns=48),
                        mock.call(rows=40, columns=140),
                    ],
                )
                self.assertEqual(invoke.call_count, 2)
                expected_steps = [
                    ("Hosts", b"\x1b[B\x1b[A\r"),
                    ("Resources", b"\x1b"),
                    ("Hosts", b"\x1b"),
                ]
                self.assertEqual(
                    interact.call_args_list,
                    [
                        mock.call("picker-wide", expected_steps),
                        mock.call("picker-narrow", expected_steps),
                    ],
                )
                self.assertEqual(wait_until.call_count, 2)
                sleep.assert_not_called()
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)

    def test_semantic_picker_steps_send_only_raw_herdr_pty_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=False,
            )
            try:
                stages = iter(["Hosts", "Resources", "Hosts"])

                def wait_without_sleep(_description, predicate, timeout=mock.ANY):
                    self.assertTrue(predicate())
                    return True

                with (
                    mock.patch.object(smoke, "pane_ids", return_value={"picker"}),
                    mock.patch.object(
                        smoke,
                        "pane_visible_text",
                        side_effect=lambda _pane: next(stages),
                    ),
                    mock.patch.object(
                        smoke, "wait_until", side_effect=wait_without_sleep
                    ),
                    mock.patch.object(smoke, "send_herdr_bytes") as send,
                    mock.patch.object(smoke, "herdr_run") as herdr_run,
                    mock.patch.object(smoke_module.time, "sleep") as sleep,
                ):
                    smoke.interact_managed_pane_via_herdr(
                        "picker",
                        [
                            ("Hosts", b"\x1b[B\x1b[A\r"),
                            ("Resources", b"\x1b"),
                            ("Hosts", b"\x1b"),
                        ],
                    )

                self.assertEqual(
                    send.call_args_list,
                    [
                        mock.call(b"\x1b[B\x1b[A\r"),
                        mock.call(b"\x1b"),
                        mock.call(b"\x1b"),
                    ],
                )
                herdr_run.assert_not_called()
                sleep.assert_not_called()
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)

    def test_cleanup_nonzero_commands_fail_but_all_attempts_continue(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=True,
            )
            owned = "tether-0123456789abcdef0123456789abcdef"
            smoke.owned_ids.add(owned)
            failed = subprocess.CompletedProcess([], 7, "sensitive stdout", "sensitive stderr")
            absent_tmux = subprocess.CompletedProcess([], 1, "", "no server running")
            empty_tether = subprocess.CompletedProcess([], 0, "[]", "")

            def run(argv, **kwargs):
                if argv[1:3] == ["list-sessions", "-F"]:
                    return absent_tmux
                if argv[1:] == ["session", "list", "--json"]:
                    return empty_tether
                return failed

            with mock.patch.object(smoke, "run", side_effect=run) as mocked_run:
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    smoke.cleanup()

            warning = stderr.getvalue()
            self.assertLess(len(warning), 1024)
            self.assertNotIn("sensitive stdout", warning)
            self.assertNotIn("sensitive stderr", warning)
            attempted = [call.args[0] for call in mocked_run.call_args_list]
            self.assertTrue(any(command[1:3] == ["session", "stop"] for command in attempted))
            self.assertTrue(any(command[1:] == ["session", "stop", smoke.session, "--json"] for command in attempted))
            self.assertTrue(any(command[1:] == ["session", "delete", smoke.session, "--json"] for command in attempted))
            self.assertEqual(smoke.cleanup_attempts, 4)
            self.assertEqual(smoke.cleanup_result, "failed")

    def test_cleanup_accepts_explicit_nonexistent_results_and_verifies_absence(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=True,
            )
            owned = "tether-0123456789abcdef0123456789abcdef"
            smoke.owned_ids.add(owned)

            def run(argv, **kwargs):
                if argv[1:3] == ["list-sessions", "-F"]:
                    return subprocess.CompletedProcess(argv, 1, "", "no server running")
                if argv[1:] == ["session", "list", "--json"]:
                    return subprocess.CompletedProcess(argv, 0, json.dumps([{"id": owned, "status": "ended"}]), "")
                return subprocess.CompletedProcess(argv, 1, "", "session is already closed")

            with mock.patch.object(smoke, "run", side_effect=run):
                smoke.cleanup()

            self.assertEqual(smoke.cleanup_attempts, 4)
            self.assertEqual(smoke.cleanup_result, "passed")

    def test_cleanup_fails_when_owned_resource_remains_after_successful_commands(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/bin/true"),
                Path("/bin/true"),
                Path(repository),
                keep=True,
            )
            owned = "tether-0123456789abcdef0123456789abcdef"
            smoke.owned_ids.add(owned)

            def run(argv, **kwargs):
                if argv[1:3] == ["list-sessions", "-F"]:
                    return subprocess.CompletedProcess(argv, 0, smoke.external_session + "\n", "")
                if argv[1:] == ["session", "list", "--json"]:
                    return subprocess.CompletedProcess(argv, 0, json.dumps([{"id": owned}]), "")
                return subprocess.CompletedProcess(argv, 0, "", "")

            with mock.patch.object(smoke, "run", side_effect=run):
                with contextlib.redirect_stderr(io.StringIO()):
                    smoke.cleanup()

            self.assertEqual(smoke.cleanup_result, "failed")

    def test_cleanup_rejects_ended_metadata_when_owned_tmux_is_still_live(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(Path("/bin/true"), Path("/bin/true"), Path(repository), keep=True)
            owned = "tether-0123456789abcdef0123456789abcdef"
            smoke.owned_ids.add(owned)

            def run(argv, **kwargs):
                if argv[1:3] == ["list-sessions", "-F"]:
                    return subprocess.CompletedProcess(argv, 0, owned + "\n", "")
                if argv[1:] == ["session", "list", "--json"]:
                    return subprocess.CompletedProcess(
                        argv, 0, json.dumps([{"id": owned, "status": "ended"}]), ""
                    )
                return subprocess.CompletedProcess(argv, 1, "", "session is already closed")

            with mock.patch.object(smoke, "run", side_effect=run):
                with contextlib.redirect_stderr(io.StringIO()):
                    smoke.cleanup()

            self.assertEqual(smoke.cleanup_result, "failed")

    def test_cleanup_failure_changes_successful_process_verdict(self) -> None:
        self.assertEqual(
            finalize_cleanup_verdict("passed", None, 0, "failed"),
            ("failed", "cleanup", 1),
        )
        self.assertEqual(
            finalize_cleanup_verdict("failed", "product_lifecycle", 1, "failed"),
            ("failed", "product_lifecycle", 1),
        )

    def test_main_returns_nonzero_and_failed_json_when_cleanup_fails(self) -> None:
        args = mock.Mock(
            herdr=Path("/bin/true"),
            tether=Path("/bin/true"),
            repo_root=Path("/tmp"),
            keep=True,
            remote_target=None,
            remote_directory=None,
            remote_known_hosts=None,
            json=True,
        )
        smoke = mock.Mock(cleanup_result="pending")
        smoke.execute.return_value = None

        def cleanup() -> None:
            smoke.cleanup_result = "failed"

        smoke.cleanup.side_effect = cleanup
        with (
            mock.patch.object(smoke_module, "parse_args", return_value=args),
            mock.patch.object(smoke_module.Smoke, "create", return_value=smoke),
            mock.patch.object(smoke_module, "smoke_report", return_value={"result": "failed"}) as report,
            mock.patch.object(smoke_module.atexit, "register"),
            mock.patch.object(smoke_module.signal, "signal"),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
        ):
            exit_code = smoke_module.main()

        self.assertEqual(exit_code, 1)
        report.assert_called_once_with(smoke, "failed", "cleanup", "failed")
        self.assertEqual(json.loads(stdout.getvalue()), {"result": "failed"})

    def test_constructor_failure_emits_json_and_removes_partial_root(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            partial_root = Path(repository) / "tether-smoke-partial"
            partial_root.mkdir()
            args = mock.Mock(
                herdr=Path("/bin/true"),
                tether=Path("/bin/true"),
                repo_root=Path(repository),
                keep=False,
                remote_target="host.example",
                remote_directory="/srv/work",
                remote_known_hosts=Path(repository) / "missing-known-hosts",
                json=True,
            )
            with (
                mock.patch.object(smoke_module, "parse_args", return_value=args),
                mock.patch.object(smoke_module.tempfile, "mkdtemp", return_value=str(partial_root)),
                mock.patch.object(smoke_module.signal, "signal"),
                contextlib.redirect_stdout(io.StringIO()) as stdout,
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                exit_code = smoke_module.main()

            report = json.loads(stdout.getvalue())
            self.assertEqual(exit_code, 1)
            self.assertEqual(report["completion"], "failed")
            self.assertIsNotNone(report["failure_category"])
            self.assertEqual(report["cleanup"], {"attempts": 0, "result": "passed"})
            self.assertEqual(stderr.getvalue(), "")
            self.assertFalse(partial_root.exists())

    def test_signal_interruption_has_distinct_json_category_and_exit_code(self) -> None:
        args = mock.Mock(
            herdr=Path("/bin/true"),
            tether=Path("/bin/true"),
            repo_root=Path("/tmp"),
            keep=False,
            remote_target=None,
            remote_directory=None,
            remote_known_hosts=None,
            json=True,
        )
        smoke = mock.Mock(
            cleanup_result="passed",
            cleanup_attempts=0,
            completed_phases=[],
            active_phase=None,
            tmux_version="unknown",
            tether_version="unknown",
        )
        smoke.execute.side_effect = SmokeInterrupted(signal.SIGTERM)
        with (
            mock.patch.object(smoke_module, "parse_args", return_value=args),
            mock.patch.object(smoke_module.Smoke, "create", return_value=smoke),
            mock.patch.object(smoke_module.atexit, "register"),
            mock.patch.object(smoke_module.signal, "signal"),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
        ):
            exit_code = smoke_module.main()

        report = json.loads(stdout.getvalue())
        self.assertEqual(exit_code, 128 + signal.SIGTERM)
        self.assertEqual(report["result"], "interrupted")
        self.assertEqual(report["failure_category"], "interrupted")

    def test_cleanup_reaches_root_removal_after_command_failures(self) -> None:
        with tempfile.TemporaryDirectory() as repository:
            smoke = Smoke(
                Path("/missing/herdr"),
                Path("/missing/tether"),
                Path(repository),
                keep=False,
            )
            smoke.env["PATH"] = ""
            root = smoke.root
            with contextlib.redirect_stderr(io.StringIO()):
                smoke.cleanup()
            self.assertFalse(root.exists())

if __name__ == "__main__":
    unittest.main()
