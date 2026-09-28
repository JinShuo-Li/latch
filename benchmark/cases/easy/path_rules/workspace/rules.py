"""Ordered glob rules for a file picker."""

from fnmatch import fnmatchcase


def should_include(path, rules):
    included = True
    for rule in rules:
        reinclude = rule.startswith("!")
        pattern = rule[1:] if reinclude else rule
        if fnmatchcase(path, pattern) and not reinclude:
            included = False
    return included
