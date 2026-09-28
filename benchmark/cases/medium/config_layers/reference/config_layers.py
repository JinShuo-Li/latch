"""Resolve CLI settings from defaults, project file, environment, and flags."""

from copy import deepcopy


def _merge(base, incoming):
    for key, value in incoming.items():
        if isinstance(value, dict) and isinstance(base.get(key), dict):
            _merge(base[key], value)
        else:
            base[key] = deepcopy(value)
    return base


def resolve_config(defaults, project, environ, flags):
    result = _merge(deepcopy(defaults), project)
    service = result.setdefault("service", {})
    if "APP_ENDPOINT" in environ:
        service["endpoint"] = environ["APP_ENDPOINT"]
    if "APP_TIMEOUT" in environ:
        service["timeout"] = int(environ["APP_TIMEOUT"])
    overlay = {}
    for key, value in flags.items():
        if value is None:
            continue
        if key in ("endpoint", "timeout"):
            overlay.setdefault("service", {})[key] = value
        else:
            overlay[key] = value
    return _merge(result, overlay)
