import unittest

from queue import Queue


class QueueTest(unittest.TestCase):
    def test_claim_and_ack(self):
        q = Queue(); q.add("a")
        job, token = q.claim(0, 10)
        self.assertEqual(job, "a")
        self.assertTrue(q.ack(job, token))
        self.assertIsNone(q.claim(11, 10))

    def test_stale_ack(self):
        q = Queue(); q.add("a")
        _, old = q.claim(0, 10)
        _, new = q.claim(11, 10)
        self.assertFalse(q.ack("a", old))
        self.assertTrue(q.ack("a", new))


if __name__ == "__main__": unittest.main()
