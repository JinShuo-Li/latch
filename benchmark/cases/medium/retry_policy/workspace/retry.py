"""Small HTTP retry loop with injected transport and sleeper."""


def request(method, send, sleep, max_attempts=3):
    response = None
    for attempt in range(max_attempts):
        response = send()
        if response["status"] not in (429, 503):
            return response
        sleep(2 ** attempt)
    return response
