import unittest

from scheduler import schedule


class SchedulerTest(unittest.TestCase):
    def test_simple_chain(self):
        calls = []
        self.assertEqual(schedule({"deploy": ["build"], "build": []}, calls.append),
                         ["build", "deploy"])

    def test_cycle_rejected(self):
        with self.assertRaises(ValueError):
            schedule({"a": ["b"], "b": ["a"]}, lambda _: None)


if __name__ == "__main__": unittest.main()
