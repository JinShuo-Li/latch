import re


FACTORS = {"ms": 1, "s": 1000, "m": 60000, "h": 3600000}


def parse_duration(value):
    match = re.fullmatch(r"(\d+)(ms|s|m|h)", value)
    if not match:
        raise ValueError("invalid duration")
    return int(match.group(1)) * FACTORS[match.group(2)]
