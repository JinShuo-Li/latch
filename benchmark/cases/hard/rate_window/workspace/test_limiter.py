import unittest

from limiter import RateLimiter


class LimiterTest(unittest.TestCase):
    def test_limit(self):
        limiter = RateLimiter(2, 10)
        self.assertTrue(limiter.allow("a", 0))
        self.assertTrue(limiter.allow("a", 1))
        self.assertFalse(limiter.allow("a", 2))

    def test_tenant_isolation(self):
        limiter = RateLimiter(1, 10)
        self.assertTrue(limiter.allow("a", 0))
        self.assertTrue(limiter.allow("b", 0))


if __name__ == "__main__": unittest.main()
