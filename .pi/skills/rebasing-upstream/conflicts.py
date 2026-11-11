"""Show and resolve git conflict blocks by their content, never by their index.

CLI: python3 conflicts.py show FILE...
Library: resolve(path, fn), where fn(ours, base, theirs) returns the replacement
text, or None to leave the block. During a rebase, "ours" is upstream (HEAD) and
"theirs" is the fork commit being applied.
"""

import re
import sys

BLOCK = re.compile(
    r"^<<<<<<< [^\n]*\n(.*?)(?:^\|\|\|\|\|\|\| [^\n]*\n(.*?))?^=======\n(.*?)^>>>>>>> [^\n]*\n",
    re.DOTALL | re.MULTILINE,
)


def blocks(path):
    return [
        (m.group(1), m.group(2) or "", m.group(3))
        for m in BLOCK.finditer(open(path).read())
    ]


def resolve(path, fn):
    text = open(path).read()
    resolved = 0

    def replace(match):
        nonlocal resolved
        result = fn(match.group(1), match.group(2) or "", match.group(3))
        if result is None:
            return match.group(0)
        resolved += 1
        return result

    text = BLOCK.sub(replace, text)
    open(path, "w").write(text)
    left = len(BLOCK.findall(text))
    print(f"{path}: resolved {resolved}, left {left}")
    return left


def show(path):
    found = blocks(path)
    print(f"#### {path}: {len(found)} blocks")
    for i, (ours, base, theirs) in enumerate(found):
        for name, side in (("upstream (HEAD)", ours), ("base", base), ("fork", theirs)):
            print(f"---- block {i} {name}")
            print(side, end="")
        print()


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] != "show":
        sys.exit("usage: conflicts.py show FILE...")
    for path in sys.argv[2:]:
        show(path)
