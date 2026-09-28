import threading
import time
import unittest

from cache import Cache


class CacheTest(unittest.TestCase):
    def test_reuses_loaded_value(self):
        cache = Cache(); calls = []
        self.assertEqual(cache.get("a", lambda: calls.append(1) or 7), 7)
        self.assertEqual(cache.get("a", lambda: 9), 7)
        self.assertEqual(calls, [1])

    def test_concurrent_single_load(self):
        cache = Cache(); calls = []; barrier = threading.Barrier(3); results = []
        def loader():
            calls.append(1); time.sleep(0.05); return 7
        def worker():
            barrier.wait(); results.append(cache.get("a", loader))
        threads = [threading.Thread(target=worker) for _ in range(2)]
        for thread in threads: thread.start()
        barrier.wait()
        for thread in threads: thread.join(timeout=2)
        self.assertEqual(results, [7, 7])
        self.assertEqual(len(calls), 1)


if __name__ == "__main__": unittest.main()
