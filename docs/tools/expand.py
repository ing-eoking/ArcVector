#!/usr/bin/env python3
"""Expand @@path:target@@ markers into verbatim code blocks pulled from the source.

target is either a line range (12-30) or an item to look up by name:

    @@src/handler/usearch/index.rs:fn insert_published@@
    @@src/handler/registry.rs:struct VectorIndex@@
    @@src/handler/usearch/index.rs:fn retire#2@@      second match
    @@src/handler/usearch/index.rs:fn search[1-12]@@  first 12 lines of it

A named lookup takes the item's attributes and the whole body, so it cannot
drift when code above it moves.  Ranges are for spans that are not one item.
"""
import re
import sys
import pathlib

ROOT = pathlib.Path("/Users/yeoncheol/Github/ArcVector")
MARK = re.compile(r"^@@([^\s:]+):(.+?)@@$")
RANGE = re.compile(r"^(\d+)-(\d+)$")
NAMED = re.compile(r"^(fn|struct|enum|impl|const|static|type|trait)\s+([A-Za-z0-9_:<>' ]+?)(?:#(\d+))?(?:\[(\d+)-(\d+)\])?$")
LEAD = re.compile(r"^\s*(#\[|#!\[|///|//!)")

cache = {}


def lines_of(rel):
    if rel not in cache:
        cache[rel] = (ROOT / rel).read_text(encoding="utf-8").split("\n")
    return cache[rel]


def opener(kind, name):
    esc = re.escape(name)
    if kind == "fn":
        return re.compile(r"^(\s*)(pub(\([^)]*\))?\s+)?(const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]+\"\s+)*"
                          rf"fn\s+{esc}\b")
    if kind == "impl":
        return re.compile(rf"^(\s*)impl(<[^>]*>)?\s+{esc}\b")
    return re.compile(rf"^(\s*)(pub(\([^)]*\))?\s+)?{kind}\s+{esc}\b")


def span(rel, kind, name, nth):
    body = lines_of(rel)
    pat = opener(kind, name)
    seen = 0
    for i, line in enumerate(body):
        m = pat.match(line)
        if not m:
            continue
        seen += 1
        if seen != nth:
            continue
        start = i
        while start > 0 and LEAD.match(body[start - 1]):
            start -= 1
        if kind in ("const", "static", "type"):
            end = i
            while end < len(body) and not body[end].rstrip().endswith(";"):
                end += 1
            return start + 1, end + 1
        depth = 0
        for j in range(i, len(body)):
            depth += body[j].count("{") - body[j].count("}")
            if depth == 0 and "{" in "".join(body[i:j + 1]):
                return start + 1, j + 1
            if depth == 0 and body[j].rstrip().endswith(";"):
                return start + 1, j + 1
        sys.exit(f"{rel}: {kind} {name} never closes")
    sys.exit(f"{rel}: no {kind} named {name}" + (f" #{nth}" if nth > 1 else ""))


def resolve(rel, target):
    r = RANGE.match(target)
    if r:
        return int(r.group(1)), int(r.group(2))
    n = NAMED.match(target)
    if not n:
        sys.exit(f"cannot read target {target!r}")
    a, b = span(rel, n.group(1), n.group(2).strip(), int(n.group(3) or 1))
    if n.group(4):
        lo, hi = int(n.group(4)), int(n.group(5))
        if a + hi - 1 > b:
            sys.exit(f"{rel}: {target} asks for line {hi} of an item {b - a + 1} lines long")
        return a + lo - 1, a + hi - 1
    return a, b


def expand(src, dst):
    out = []
    for n, line in enumerate(pathlib.Path(src).read_text(encoding="utf-8").split("\n"), 1):
        m = MARK.match(line.strip())
        if not m:
            out.append(line)
            continue
        rel, target = m.group(1), m.group(2)
        a, b = resolve(rel, target)
        body = lines_of(rel)[a - 1:b]
        if not body:
            sys.exit(f"{src}:{n}: {rel}:{target} is empty")
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
