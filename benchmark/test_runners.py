#!/usr/bin/env python3
"""Offline checks for benchmark runner report-directory handling.

Runs without credentials, network, or sandbox prerequisites:
`python3 benchmark/test_runners.py`.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run  # noqa: E402


class ReportRootTest(unittest.TestCase):
    def test_existing_output_directory_is_reused(self):
        with tempfile.TemporaryDirectory() as parent:
            container = Path(parent) / "results"
            container.mkdir()
            first, first_stamp = run.report_root(container, "comparison")
            second, second_stamp = run.report_root(container, "comparison")
            self.assertTrue(first.is_dir() and second.is_dir())
            self.assertNotEqual(first, second, "each run needs its own directory")
            self.assertEqual(first.parent, container.resolve())
            self.assertEqual(second.parent, container.resolve())
            self.assertTrue(first.name.startswith("comparison-"), first.name)
            self.assertEqual(len(first_stamp), 16)
            self.assertEqual(len(second_stamp), 16)

    def test_missing_output_directory_is_created(self):
        with tempfile.TemporaryDirectory() as parent:
            container = Path(parent) / "nested" / "results"
            root, _ = run.report_root(container, "opencode")
            self.assertTrue(root.is_dir())
            self.assertEqual(root.parent, container.resolve())
            self.assertTrue(root.name.startswith("opencode-"), root.name)

    def test_unlabelled_root_keeps_the_run_runner_naming(self):
        with tempfile.TemporaryDirectory() as parent:
            container = Path(parent) / "runs"
            root, stamp = run.report_root(container, "")
            self.assertEqual(root.name, f"{stamp}-{os.getpid()}")

    @unittest.skipUnless(os.name == "posix", "POSIX permission bits")
    def test_report_directories_are_private(self):
        with tempfile.TemporaryDirectory() as parent:
            container = Path(parent) / "results"
            root, _ = run.report_root(container, "comparison")
            self.assertEqual(root.stat().st_mode & 0o777, 0o700)
            self.assertEqual(container.stat().st_mode & 0o777, 0o700)


if __name__ == "__main__":
    unittest.main(verbosity=2)
