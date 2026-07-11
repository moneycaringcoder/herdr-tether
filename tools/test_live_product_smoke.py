#!/usr/bin/env python3
import contextlib
import io
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock

from live_product_smoke import Smoke, terminal_screen_text


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
                tether_close_commands = [
                    command
                    for command in commands
                    if command[1:3] == ["session", "close"]
                ]
                self.assertEqual(
                    tether_close_commands,
                    [[str(smoke.tether), "session", "close", valid_id]],
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
