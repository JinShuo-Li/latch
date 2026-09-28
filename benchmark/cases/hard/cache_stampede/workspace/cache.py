"""Concurrent in-process value cache."""


class Cache:
    def __init__(self):
        self.values = {}

    def get(self, key, loader):
        if key not in self.values:
            self.values[key] = loader()
        return self.values[key]
