#!/usr/bin/env python3
import contextlib
import io
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock

from live_product_smoke import Smoke


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
