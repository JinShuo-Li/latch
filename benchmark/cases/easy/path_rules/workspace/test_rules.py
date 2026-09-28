import unittest

from rules import should_include


class RulesTest(unittest.TestCase):
    def test_unmatched_is_included(self):
        self.assertTrue(should_include("src/main.py", ["*.log"]))

    def test_later_rule_reincludes(self):
        self.assertTrue(should_include("logs/keep.log", ["logs/*", "!logs/keep.log"]))


if __name__ == "__main__":
    unittest.main()
