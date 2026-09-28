import re
import unicodedata


ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def display_width(text):
    clean = ANSI.sub("", text)
    return sum(0 if unicodedata.combining(char) or unicodedata.category(char) in ("Cf", "Cc")
               else 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
               for char in clean)
