"""Normalize incoming gateway headers."""


def normalize_headers(pairs):
    result = {}
    for name, value in pairs:
        result[name] = value
    return result
