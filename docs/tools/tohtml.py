#!/usr/bin/env python3
"""Render the review guide's markdown into the artifact page.

Handles exactly the constructs the source uses: headings, paragraphs, hr, fenced
rust code preceded by a `**[file:lines](url)**` caption, tables, bullet lists,
ordered lists, blockquotes, and runs of `☐` lines (which become checklist cards).
"""
import html
import re
import sys
import pathlib

INLINE_CODE = re.compile(r"`([^`]+)`")
BOLD = re.compile(r"\*\*(.+?)\*\*")
ITALIC = re.compile(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])")
LINK = re.compile(r"\[([^\]]+)\]\(([^)]+)\)")
CAPTION = re.compile(r"^\*\*\[(.+?):(\d+)-(\d+)\]\((.+?)\)\*\*$")
HEADING = re.compile(r"^(#{1,3}) +(.*)$")


def inline(text):
    out = html.escape(text)
    # Links first: their labels may contain code spans.
    out = LINK.sub(lambda m: f'<a href="{m.group(2)}">{m.group(1)}</a>', out)
    out = INLINE_CODE.sub(lambda m: f"<code>{m.group(1)}</code>", out)
    out = BOLD.sub(lambda m: f"<strong>{m.group(1)}</strong>", out)
    out = ITALIC.sub(lambda m: f"<em>{m.group(1)}</em>", out)
    return out


def slug(text, seen):
    base = re.sub(r"[^0-9A-Za-z가-힣]+", "-", re.sub(r"[`*]", "", text)).strip("-").lower()
    base = base or "s"
    n, out = 1, base
    while out in seen:
        n += 1
        out = f"{base}-{n}"
    seen.add(out)
    return out


def render(md):
    lines = md.split("\n")
    body, toc, seen = [], [], set()
    i, n = 0, len(lines)

    while i < n:
        line = lines[i]
        stripped = line.strip()

        if not stripped:
            i += 1
            continue

        if stripped == "---":
            body.append('<hr class="rule">')
            i += 1
            continue

        m = HEADING.match(stripped)
        if m:
            level, text = len(m.group(1)), m.group(2)
            anchor = slug(text, seen)
            num = re.match(r"^((?:\d+부|부록|\d+(?:\.\d+)?|I\d))[.·]? +(.*)$", text)
            if num:
                label, rest = num.group(1), num.group(2).lstrip("· ")
                inner = f'<span class="num">{html.escape(label)}</span>{inline(rest)}'
            else:
                inner = inline(text)
            if level == 1:
                body.append(f'<h2 id="{anchor}" class="part">{inner}</h2>')
                toc.append((1, anchor, text))
            elif level == 2:
                body.append(f'<h3 id="{anchor}">{inner}</h3>')
                toc.append((2, anchor, text))
            else:
                body.append(f'<h4 id="{anchor}">{inner}</h4>')
            i += 1
            continue

        # A code card: caption line, blank, fence.
        cap = CAPTION.match(stripped)
        if cap and i + 2 < n and lines[i + 2].strip().startswith("```"):
            where, a, b, href = cap.group(1), cap.group(2), cap.group(3), cap.group(4)
            i += 3
            code = []
            while i < n and lines[i].strip() != "```":
                code.append(html.escape(lines[i]))
                i += 1
            i += 1
            _ = href  # the repo is not reachable from a published page
            body.append(
                '<figure class="code">'
                f'<figcaption><span class="file">{html.escape(where)}</span>'
                f'<span class="ln">{a}–{b}</span></figcaption>'
                f'<pre><code>{chr(10).join(code)}</code></pre></figure>'
            )
            continue

        if stripped.startswith("```"):
            fence = stripped[3:] or "text"
            i += 1
            code = []
            while i < n and lines[i].strip() != "```":
                code.append(html.escape(lines[i]))
                i += 1
            i += 1
            body.append(
                f'<figure class="code plain"><pre><code class="{fence}">'
                f"{chr(10).join(code)}</code></pre></figure>"
            )
            continue

        if stripped.startswith("|"):
            rows = []
            while i < n and lines[i].strip().startswith("|"):
                rows.append([c.strip() for c in lines[i].strip().strip("|").split("|")])
                i += 1
            head, rest = rows[0], rows[2:]
            thead = "".join(f"<th>{inline(c)}</th>" for c in head)
            tbody = "".join(
                "<tr>" + "".join(f"<td>{inline(c)}</td>" for c in r) + "</tr>" for r in rest
            )
            body.append(
                f'<div class="scroll"><table><thead><tr>{thead}</tr></thead>'
                f"<tbody>{tbody}</tbody></table></div>"
            )
            continue

        if stripped.startswith("☐"):
            items = []
            while i < n and lines[i].strip().startswith("☐"):
                item = [lines[i].strip()[1:].strip()]
                i += 1
                while i < n and lines[i].startswith("  ") and not lines[i].strip().startswith("☐"):
                    item.append(lines[i].strip())
                    i += 1
                items.append(" ".join(item))
            lis = "".join(f"<li>{inline(t)}</li>" for t in items)
            body.append(f'<ul class="checks">{lis}</ul>')
            continue

        if stripped.startswith("> "):
            quote = []
            while i < n and lines[i].strip().startswith("> "):
                quote.append(lines[i].strip()[2:])
                i += 1
            body.append(f'<blockquote>{inline(" ".join(quote))}</blockquote>')
            continue

        if stripped.startswith("- "):
            items = []
            while i < n and lines[i].strip().startswith("- "):
                item = [lines[i].strip()[2:]]
                i += 1
                while i < n and lines[i].startswith("  ") and lines[i].strip():
                    item.append(lines[i].strip())
                    i += 1
                items.append(" ".join(item))
            lis = "".join(f"<li>{inline(t)}</li>" for t in items)
            body.append(f"<ul>{lis}</ul>")
            continue

        if re.match(r"^\d+\. ", stripped):
            items = []
            while i < n and re.match(r"^\d+\. ", lines[i].strip()):
                items.append(re.sub(r"^\d+\. ", "", lines[i].strip()))
                i += 1
            lis = "".join(f"<li>{inline(t)}</li>" for t in items)
            body.append(f"<ol>{lis}</ol>")
            continue

        para = [stripped]
        i += 1
        while i < n and lines[i].strip() and not re.match(
            r"^(#{1,3} |---$|```|\||☐|> |- |\d+\. |\*\*\[)", lines[i].strip()
        ):
            para.append(lines[i].strip())
            i += 1
        body.append(f"<p>{inline(' '.join(para))}</p>")

    nav = []
    for level, anchor, text in toc:
        cls = "part" if level == 1 else "sec"
        label = re.sub(r"[`*]", "", text)
        nav.append(f'<a class="{cls}" href="#{anchor}">{html.escape(label)}</a>')
    return "\n".join(body), "\n".join(nav)


SHELL = """<title>ArcVector 코드 리뷰 가이드</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Sans+KR:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;700&display=swap">

<style>
  :root {{
    --ground: #f1f3ef;
    --surface: #ffffff;
    --sunk: #eaece7;
    --ink: #16191c;
    --muted: #5b6470;
    --line: #ccd2cd;
    --line-soft: #e0e4de;

    --arcus: #0d6b6b;
    --usearch: #4a52a6;
    --fail: #a03636;
    --fail-wash: #f5e6e6;
    --check: #4e6b34;
    --check-wash: #edf1e7;

    --sans: "IBM Plex Sans KR", system-ui, -apple-system, sans-serif;
    --mono: "JetBrains Mono", ui-monospace, "SF Mono", Menlo, monospace;
  }}

  @media (prefers-color-scheme: dark) {{
    :root:not([data-theme="light"]) {{
      --ground: #14171a;
      --surface: #1b1f23;
      --sunk: #101316;
      --ink: #e2e6e9;
      --muted: #949da8;
      --line: #2b3138;
      --line-soft: #232930;

      --arcus: #47b3ae;
      --usearch: #939ce0;
      --fail: #d98181;
      --fail-wash: #2e1e1e;
      --check: #a8c785;
      --check-wash: #1d2419;
    }}
  }}

  :root[data-theme="dark"] {{
    --ground: #14171a;
    --surface: #1b1f23;
    --sunk: #101316;
    --ink: #e2e6e9;
    --muted: #949da8;
    --line: #2b3138;
    --line-soft: #232930;

    --arcus: #47b3ae;
    --usearch: #939ce0;
    --fail: #d98181;
    --fail-wash: #2e1e1e;
    --check: #a8c785;
    --check-wash: #1d2419;
  }}

  * {{ box-sizing: border-box; }}

  body {{
    margin: 0;
    background: var(--ground);
    color: var(--ink);
    font-family: var(--sans);
    font-size: 16px;
    line-height: 1.75;
    -webkit-font-smoothing: antialiased;
  }}

  .page {{
    max-width: 1240px;
    margin: 0 auto;
    padding: 0 24px 120px;
    display: grid;
    grid-template-columns: 236px minmax(0, 1fr);
    gap: 48px;
    align-items: start;
  }}

  /* ---- masthead ---- */

  header {{
    grid-column: 1 / -1;
    padding: 56px 0 32px;
    border-bottom: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    gap: 14px;
  }}

  .eyebrow {{
    font-family: var(--mono);
    font-size: 11.5px;
    font-weight: 500;
    letter-spacing: 0.18em;
    text-transform: uppercase;
    color: var(--check);
  }}

  h1 {{
    font-family: var(--sans);
    font-weight: 700;
    font-size: clamp(28px, 4vw, 40px);
    line-height: 1.2;
    letter-spacing: -0.02em;
    margin: 0;
    text-wrap: balance;
  }}

  .lede {{ margin: 0; max-width: 64ch; color: var(--muted); font-size: 16.5px; }}

  /* ---- contents rail ---- */

  nav {{
    position: sticky;
    top: 24px;
    max-height: calc(100vh - 48px);
    overflow-y: auto;
    padding: 28px 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    font-size: 13.5px;
    line-height: 1.5;
  }}

  nav a {{
    color: var(--muted);
    text-decoration: none;
    padding: 3px 10px;
    border-left: 2px solid transparent;
  }}
  nav a:hover {{ color: var(--ink); border-left-color: var(--line); }}
  nav a.part {{
    font-family: var(--mono);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--ink);
    margin-top: 18px;
    padding-left: 10px;
  }}
  nav a.part:first-child {{ margin-top: 0; }}

  /* ---- article ---- */

  article {{
    padding: 28px 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 18px;
  }}

  h2.part {{
    font-family: var(--mono);
    font-weight: 700;
    font-size: 13px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--check);
    margin: 40px 0 0;
    padding-bottom: 10px;
    border-bottom: 2px solid var(--check);
  }}
  h2.part:first-child {{ margin-top: 0; }}

  h3 {{
    font-weight: 600;
    font-size: 25px;
    line-height: 1.3;
    letter-spacing: -0.015em;
    margin: 34px 0 0;
    text-wrap: balance;
  }}

  h4 {{
    font-weight: 600;
    font-size: 16.5px;
    margin: 22px 0 0;
    color: var(--ink);
  }}

  h3 .num, h4 .num, h2.part .num {{
    font-family: var(--mono);
    font-weight: 700;
    color: var(--muted);
    margin-right: 10px;
    font-size: 0.82em;
    letter-spacing: 0;
  }}
  h2.part .num {{ color: inherit; margin-right: 8px; }}

  p {{ margin: 0; max-width: 74ch; }}

  a {{ color: var(--usearch); text-decoration-thickness: 1px; text-underline-offset: 2px; }}

  strong {{ font-weight: 600; }}

  code {{
    font-family: var(--mono);
    font-size: 0.86em;
    background: var(--sunk);
    border: 1px solid var(--line-soft);
    border-radius: 2px;
    padding: 1px 4px;
  }}

  blockquote {{
    margin: 0;
    padding: 12px 16px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 3px solid var(--arcus);
    color: var(--muted);
    font-size: 14.5px;
    max-width: 74ch;
  }}
  blockquote code {{ background: none; border: 0; padding: 0; }}

  hr.rule {{ height: 1px; background: var(--line); border: 0; margin: 22px 0 4px; width: 100%; }}

  ul, ol {{ margin: 0; padding-left: 22px; max-width: 74ch; display: flex; flex-direction: column; gap: 6px; }}

  /* ---- checklist ---- */

  ul.checks {{
    list-style: none;
    padding: 14px 16px 14px 18px;
    margin: 4px 0;
    background: var(--check-wash);
    border-left: 3px solid var(--check);
    gap: 9px;
    font-size: 15px;
  }}
  ul.checks li {{ position: relative; padding-left: 26px; }}
  ul.checks li::before {{
    content: "";
    position: absolute;
    left: 0;
    top: 0.45em;
    width: 13px;
    height: 13px;
    border: 1.5px solid var(--check);
    border-radius: 2px;
  }}
  ul.checks code {{ background: var(--surface); border-color: var(--line-soft); }}

  /* ---- code ---- */

  figure.code {{ margin: 6px 0; min-width: 0; }}

  figcaption {{
    font-family: var(--mono);
    font-size: 11.5px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-bottom: 0;
    padding: 6px 12px;
  }}
  figcaption {{ display: flex; gap: 10px; color: var(--muted); }}
  figcaption .file {{ color: var(--ink); font-weight: 500; }}
  figcaption .ln {{ font-variant-numeric: tabular-nums; }}

  pre {{
    margin: 0;
    overflow-x: auto;
    background: var(--sunk);
    border: 1px solid var(--line);
    padding: 14px 16px;
    font-size: 12.5px;
    line-height: 1.62;
  }}
  pre code {{ background: none; border: 0; padding: 0; font-size: inherit; }}
  figure.plain pre {{ background: var(--surface); }}

  /* ---- tables ---- */

  .scroll {{ overflow-x: auto; border: 1px solid var(--line); background: var(--surface); max-width: 100%; }}
  table {{ border-collapse: collapse; width: 100%; min-width: 560px; font-size: 14.5px; }}
  thead th {{
    text-align: left;
    font-family: var(--mono);
    font-weight: 500;
    font-size: 11px;
    letter-spacing: 0.11em;
    text-transform: uppercase;
    color: var(--muted);
    padding: 10px 14px;
    border-bottom: 1px solid var(--line);
    white-space: nowrap;
  }}
  tbody td {{ padding: 11px 14px; border-bottom: 1px solid var(--line-soft); vertical-align: top; }}
  tbody tr:last-child td {{ border-bottom: 0; }}
  td code {{ background: none; border: 0; padding: 0; }}

  :focus-visible {{ outline: 2px solid var(--check); outline-offset: 2px; }}

  @media (max-width: 900px) {{
    .page {{ grid-template-columns: minmax(0, 1fr); gap: 0; }}
    nav {{
      position: static;
      max-height: none;
      border-bottom: 1px solid var(--line);
      padding: 20px 0;
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
      gap: 2px 16px;
    }}
  }}
</style>

<div class="page">
  <header>
    <div class="eyebrow">ArcVector · 리뷰 가이드</div>
    <h1>코드를 옆에 두고 읽는 문서</h1>
    <p class="lede">
      절마다 실제 소스 조각 하나와, 그것이 왜 그렇게 생겼는지, 그리고 리뷰어가 확인할 것을
      짝지어 놓았습니다. 코드 블록은 전부 소스에서 줄 범위째로 뽑았고 손으로 옮긴 것은
      없습니다. 체크박스가 확인 항목이며, 근거가 코드 안에 있는 것만 적었습니다.
    </p>
  </header>

  <nav>
{nav}
  </nav>

  <article>
{body}
  </article>
</div>
"""


def main(src, dst):
    md = pathlib.Path(src).read_text(encoding="utf-8")
    # The masthead carries the title and the reading instructions; keep the rest.
    keep = md.find("> **기준 커밋**")
    if keep > 0:
        md = md[keep:]
    # Repo-relative links go nowhere from a published page.
    md = re.sub(r"\[([^\]]+)\]\((?:\.\./)?[^)]*\.md\)", r"`\1`", md)
    body, nav = render(md)
    pathlib.Path(dst).write_text(SHELL.format(nav=nav, body=body), encoding="utf-8")
    print(f"wrote {dst}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
