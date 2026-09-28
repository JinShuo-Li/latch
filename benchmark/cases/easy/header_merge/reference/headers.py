def normalize_headers(pairs):
    result = {}
    for name, value in pairs:
        key = name.lower()
        if key == "set-cookie":
            result.setdefault(key, []).append(value)
        elif key in result:
            result[key] += ", " + value
        else:
            result[key] = value
    return result
