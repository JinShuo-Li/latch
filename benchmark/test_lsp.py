#!/usr/bin/env python3
"""Offline checks for the paid LSP pilot; never calls a provider."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import run_lsp


class LspPilotTests(unittest.TestCase):
    def test_paid_guard_precedes_config_or_credential_access(self):
        result = subprocess.run([sys.executable, str(Path(run_lsp.__file__)), "--source-config", "/does/not/exist"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("requires --live", result.stderr)

    def test_credential_is_not_materialized_in_config(self):
        with tempfile.TemporaryDirectory() as root:
            secret = Path(root) / "secrets.toml"
            secret.write_text('[secrets]\nfixture = "private-test-key"\n')
            provider = {"kind": "opencode-go", "credential": "file:fixture"}
            value = run_lsp.credential({}, provider, secret)
            text = run_lsp.config_text("go", provider, Path(root) / "state", "/usr/bin/clangd")
            self.assertEqual(value, "private-test-key")
            self.assertNotIn(value, text)
            parsed = __import__("tomllib").loads(text)
            self.assertEqual(parsed["providers"]["go"]["credential"], "env:LATCH_LSP_LIVE_KEY")
            self.assertEqual(parsed["failure"]["max_model_turns"], 12)
            self.assertEqual(parsed["permissions"]["outside_workspace"], "deny")
        with patch.dict(os.environ, {"FIXTURE_LSP_KEY": "env-secret"}):
            self.assertEqual(run_lsp.credential({}, {"credential": "env:FIXTURE_LSP_KEY"}, None), "env-secret")

    def test_navigation_requires_a_user_answer_not_only_completion(self):
        self.assertFalse(run_lsp.has_navigation_answer("Kernel completion report: no changed files"))
        self.assertFalse(run_lsp.has_navigation_answer("I'll inspect quote references"))
        self.assertTrue(run_lsp.has_navigation_answer("main.cpp:2 declares int quote(int), called at main.cpp:6; double quote is separate"))

    def test_event_extraction_matches_durable_protocol(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root)
            with sqlite3.connect(state / "latch.sqlite3") as connection:
                connection.execute("CREATE TABLE events(kind TEXT,payload TEXT)")
                for kind, data in [("tool_requested", {"call": {"id": "1", "name": "lsp", "arguments": {"action": "symbols"}}}),
                                   ("tool_completed", {"result": {"call_id": "1", "is_error": False}}),
                                   ("validation_result", {"passed": True})]:
                    connection.execute("INSERT INTO events VALUES(?,?)", (kind, json.dumps({"type": kind, "data": data})))
            calls, results, validations = run_lsp.events(state)
            self.assertEqual(calls[0]["name"], "lsp")
            self.assertFalse(results["1"]["is_error"])
            self.assertTrue(validations[0]["passed"])

    def test_independent_harness_rejects_original_rate_and_accepts_repair(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            for name, text, expected in [("wrong", run_lsp.CODE, False),
                                         ("fixed", run_lsp.CODE.replace("n * 10", "n * 12").replace("cents == 30", "cents == 36"), True)]:
                case = root / name
                workspace = case / "workspace"
                workspace.mkdir(parents=True)
                (workspace / "main.cpp").write_text(text)
                self.assertEqual(run_lsp.independent_acceptance(case, workspace), expected)


if __name__ == "__main__":
    unittest.main()
