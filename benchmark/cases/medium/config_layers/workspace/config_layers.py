"""Resolve CLI settings from defaults, project file, environment, and flags."""

from copy import deepcopy


def resolve_config(defaults, project, environ, flags):
    """Return merged settings; later sources take precedence.

    ``None`` in flags means absent, while an empty string is an explicit value.
    APP_ENDPOINT and APP_TIMEOUT override the corresponding nested settings.
    """
    result = deepcopy(defaults)
    result.update(project)
    service = result.setdefault("service", {})
    if "APP_ENDPOINT" in environ:
        service["endpoint"] = environ["APP_ENDPOINT"]
    if "APP_TIMEOUT" in environ:
        service["timeout"] = int(environ["APP_TIMEOUT"])
    for key, value in flags.items():
        if value:
            if key in ("endpoint", "timeout"):
                service[key] = value
            else:
                result[key] = value
    return result
