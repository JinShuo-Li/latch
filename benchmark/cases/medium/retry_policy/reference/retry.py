def request(method, send, sleep, max_attempts=3):
    if max_attempts < 1:
        raise ValueError("max_attempts must be positive")
    for attempt in range(max_attempts):
        response = send()
        if (method.upper() not in ("GET", "HEAD") or
                response["status"] not in (429, 503) or attempt == max_attempts - 1):
            return response
        header = response.get("headers", {}).get("Retry-After")
        delay = 2 ** attempt
        if header is not None:
            try:
                parsed = float(header)
                if parsed >= 0:
                    delay = parsed
            except ValueError:
                pass
        sleep(delay)
