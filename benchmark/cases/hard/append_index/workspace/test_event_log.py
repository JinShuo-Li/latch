import tempfile
import unittest
from pathlib import Path

from event_log import EventLog


class EventLogTest(unittest.TestCase):
    def test_round_trip(self):
        with tempfile.TemporaryDirectory() as directory:
            log = EventLog(Path(directory) / "events")
            self.assertEqual(log.append({"id": 1}), 0)
            self.assertEqual(log.read_all(), [{"id": 1}])

    def test_byte_offsets(self):
        with tempfile.TemporaryDirectory() as directory:
            log = EventLog(Path(directory) / "events")
            log.append({"name": "雪"}); second = log.append({"id": 2})
            self.assertEqual(log.rebuild_index(), [0, second])


if __name__ == "__main__": unittest.main()
