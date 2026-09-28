import tempfile
import unittest
from pathlib import Path

from spool_recovery import process_spool


class SpoolRecoveryTest(unittest.TestCase):
    def test_normal_run_and_resume(self):
        with tempfile.TemporaryDirectory() as directory:
            spool = Path(directory) / "jobs.jsonl"
            checkpoint = Path(directory) / "offset"
            spool.write_bytes(b'{"id":1}\n{"id":2}\n')
            seen = []
            self.assertEqual(process_spool(spool, checkpoint, seen.append), 2)
            self.assertEqual(process_spool(spool, checkpoint, seen.append), 0)
            self.assertEqual(seen, [{"id": 1}, {"id": 2}])

    def test_failed_handler_is_retried(self):
        with tempfile.TemporaryDirectory() as directory:
            spool = Path(directory) / "jobs.jsonl"
            checkpoint = Path(directory) / "offset"
            spool.write_bytes(b'{"id":1}\n')

            def fail(_job):
                raise RuntimeError("downstream unavailable")

            with self.assertRaises(RuntimeError):
                process_spool(spool, checkpoint, fail)
            seen = []
            self.assertEqual(process_spool(spool, checkpoint, seen.append), 1)
            self.assertEqual(seen, [{"id": 1}])

    def test_partial_line_waits_for_append(self):
        with tempfile.TemporaryDirectory() as directory:
            spool = Path(directory) / "jobs.jsonl"
            checkpoint = Path(directory) / "offset"
            spool.write_bytes(b'{"id":1}\n{"id":')
            seen = []
            self.assertEqual(process_spool(spool, checkpoint, seen.append), 1)
            with spool.open("ab") as stream:
                stream.write(b'2}\n')
            self.assertEqual(process_spool(spool, checkpoint, seen.append), 1)
            self.assertEqual(seen, [{"id": 1}, {"id": 2}])


if __name__ == "__main__":
    unittest.main()
