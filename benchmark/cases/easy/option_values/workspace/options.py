"""Arguments for a small deployment command."""


def parse_args(argv):
    result = {"label": None, "quiet": False, "files": []}
    index = 0
    while index < len(argv):
        arg = argv[index]
        if arg == "--quiet":
            result["quiet"] = True
        elif arg == "--label":
            index += 1
            if index >= len(argv):
                raise ValueError("--label needs a value")
            if argv[index]:
                result["label"] = argv[index]
        elif arg.startswith("-"):
            raise ValueError(f"unknown option: {arg}")
        else:
            result["files"].append(arg)
        index += 1
    return result
