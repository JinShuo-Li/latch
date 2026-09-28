import unittest

from retry import request


class RetryTest(unittest.TestCase):
    def test_success(self):
        self.assertEqual(request("GET", lambda: {"status": 200}, lambda _: None),
                         {"status": 200})

    def test_post_is_not_retried(self):
        calls = []
        result = request("POST", lambda: calls.append(1) or {"status": 503},
                         lambda _: None)
        self.assertEqual(result["status"], 503)
        self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
