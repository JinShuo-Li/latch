"""Scheduler duration parser returning milliseconds."""


def parse_duration(value):
    for unit, factor in (("s", 1000), ("m", 60000), ("h", 3600000), ("ms", 1)):
        if unit in value:
            return int(value.replace(unit, "", 1)) * factor
    raise ValueError("unknown duration")
