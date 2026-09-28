def parse_args(argv):
    result = {"label": None, "quiet": False, "files": []}
    index = 0
    positional = False
    while index < len(argv):
        arg = argv[index]
        if positional:
            result["files"].append(arg)
        elif arg == "--":
            positional = True
        elif arg == "--quiet":
            result["quiet"] = True
        elif arg == "--label":
            index += 1
            if index >= len(argv):
                raise ValueError("--label needs a value")
            result["label"] = argv[index]
        elif arg.startswith("-"):
            raise ValueError(f"unknown option: {arg}")
        else:
            result["files"].append(arg)
        index += 1
    return result
