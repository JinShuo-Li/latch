"""In-memory gateway rate limiter."""


class RateLimiter:
    def __init__(self, max_requests, window):
        self.max_requests = max_requests
        self.window = window
        self.timestamps = []

    def allow(self, tenant, now):
        self.timestamps = [stamp for stamp in self.timestamps if now - stamp <= self.window]
        if len(self.timestamps) >= self.max_requests:
            return False
        self.timestamps.append(now)
        return True
