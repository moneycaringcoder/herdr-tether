#!/usr/bin/env python3

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
                {"TMUX": "/tmp/tmux-parent/default,123,0"},
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
                tmux_root = Path(smoke.env["TMUX_TMPDIR"])
                self.assertTrue(tmux_root.is_relative_to(smoke.root))
                self.assertTrue(tmux_root.is_dir())
            finally:
                shutil.rmtree(smoke.root, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
