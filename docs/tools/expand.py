#!/usr/bin/env python3
"""Expand @@path:a-b@@ markers into verbatim code blocks pulled from the source."""
import re
import sys
import pathlib

ROOT = pathlib.Path("/Users/yeoncheol/Github/ArcVector")
MARK = re.compile(r"^@@([^\s:]+):(\d+)-(\d+)@@$")

cache = {}


def lines_of(rel):
    if rel not in cache:
        cache[rel] = (ROOT / rel).read_text(encoding="utf-8").split("\n")
    return cache[rel]


def expand(src, dst):
    out = []
    for n, line in enumerate(pathlib.Path(src).read_text(encoding="utf-8").split("\n"), 1):
        m = MARK.match(line.strip())
        if not m:
            out.append(line)
            continue
        rel, a, b = m.group(1), int(m.group(2)), int(m.group(3))
        body = lines_of(rel)[a - 1:b]
        if not body:
            sys.exit(f"{src}:{n}: {rel}:{a}-{b} is empty")
        short = rel.split("src/handler/", 1)[-1]
        anchor = f"#L{a}-L{b}" if b > a else f"#L{a}"
        out.append(f"**[{short}:{a}-{b}](../{rel}{anchor})**")
        out.append("")
        out.append("```rust")
        out.extend(body)
        out.append("```")
    pathlib.Path(dst).write_text("\n".join(out), encoding="utf-8")
    print(f"wrote {dst}: {len(out)} lines")


if __name__ == "__main__":
    expand(sys.argv[1], sys.argv[2])
