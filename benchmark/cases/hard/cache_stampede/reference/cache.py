import threading


class Cache:
    def __init__(self):
        self.values = {}
        self.guard = threading.Lock()
        self.locks = {}

    def get(self, key, loader):
        with self.guard:
            if key in self.values:
                return self.values[key]
            lock = self.locks.setdefault(key, threading.Lock())
        with lock:
            with self.guard:
                if key in self.values:
                    return self.values[key]
            value = loader()
            with self.guard:
                self.values[key] = value
            return value
