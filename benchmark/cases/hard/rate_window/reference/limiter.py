class RateLimiter:
    def __init__(self, max_requests, window):
        if max_requests <= 0 or window <= 0:
            raise ValueError("positive limit and window required")
        self.max_requests = max_requests
        self.window = window
        self.timestamps = {}

    def allow(self, tenant, now):
        active = [stamp for stamp in self.timestamps.get(tenant, []) if now - stamp < self.window]
        self.timestamps[tenant] = active
        if len(active) >= self.max_requests:
            return False
        active.append(now)
        return True
