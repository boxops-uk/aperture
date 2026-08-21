#!/usr/bin/env python3
"""Build the Fjord documentation site.

Reads `content/*.md`, renders each page into one HTML shell, and writes the
result to `site/`. Standard library only — no toolchain, no lockfile, no
network. `python3 build.py` is the whole build.

The markdown dialect is deliberately small: headings, paragraphs, fenced code,
lists, pipe tables, blockquotes, `:::note` callouts, horizontal rules, and the
usual inline marks. It is exactly what the content in `content/` uses.
"""

from __future__ import annotations

import html
import json
import re
import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CONTENT = ROOT / "content"
ASSETS = ROOT / "assets"
OUT = ROOT / "site"

SITE_TITLE = "Fjord DB"
SITE_TAGLINE = "An embedded, immutable fact database"

# The navigation is the reading order, and `nav.json` is the only copy of it —
# the interactive site in `web/` renders its sidebar from the same file, and a
# second list here would be a second reading order nobody edits twice.
NAV: list[tuple[str, list[tuple[str, str]]]] = [
    (group["label"], [(page["slug"], page["title"]) for page in group["pages"]])
    for group in json.loads((ROOT / "nav.json").read_text(encoding="utf-8"))["groups"]
]

ORDER = [slug for _, pages in NAV for slug, _ in pages]


# --------------------------------------------------------------------------- #
# inline marks
# --------------------------------------------------------------------------- #

CODE_SPAN = re.compile(r"`([^`]+)`")
LINK = re.compile(r"\[([^\]]+)\]\(([^)\s]+)\)")
BOLD = re.compile(r"\*\*(.+?)\*\*")
ITALIC = re.compile(r"(?<![\*\w])\*([^*\n]+)\*(?!\*)")
STRIKE = re.compile(r"~~(.+?)~~")
SENTINEL = "\x00{}\x00"


def inline(text: str) -> str:
    """Render inline marks. Code spans are lifted out before escaping."""
    spans: list[str] = []

    def stash(match: re.Match[str]) -> str:
        spans.append(html.escape(match.group(1), quote=False))
        return SENTINEL.format(len(spans) - 1)

    text = CODE_SPAN.sub(stash, text)
    text = html.escape(text, quote=False)
    text = LINK.sub(lambda m: f'<a href="{html.escape(m.group(2), quote=True)}">{m.group(1)}</a>', text)
    text = BOLD.sub(r"<strong>\1</strong>", text)
    text = ITALIC.sub(r"<em>\1</em>", text)
    text = STRIKE.sub(r"<del>\1</del>", text)

    for index, code in enumerate(spans):
        text = text.replace(SENTINEL.format(index), f"<code>{code}</code>")
    return text


def plain(text: str) -> str:
    """The same text with every mark removed — for the search index."""
    text = CODE_SPAN.sub(r"\1", text)
    text = LINK.sub(r"\1", text)
    text = BOLD.sub(r"\1", text)
    text = ITALIC.sub(r"\1", text)
    text = STRIKE.sub(r"\1", text)
    return text.strip()


def slugify(text: str) -> str:
    text = plain(text).lower()
    text = re.sub(r"[^a-z0-9\s-]", "", text)
    return re.sub(r"[\s-]+", "-", text).strip("-") or "section"


# --------------------------------------------------------------------------- #
# block structure
# --------------------------------------------------------------------------- #


@dataclass
class Page:
    slug: str
    title: str
    description: str = ""
    body: str = ""
    toc: list[tuple[int, str, str]] = field(default_factory=list)
    search: list[dict] = field(default_factory=list)


def parse_front_matter(text: str) -> tuple[dict, str]:
    if not text.startswith("---\n"):
        return {}, text
    end = text.find("\n---\n", 4)
    if end == -1:
        return {}, text
    meta = {}
    for line in text[4:end].splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            meta[key.strip()] = value.strip()
    return meta, text[end + 5 :]


def render(source: str, page: Page) -> str:
    lines = source.split("\n")
    out: list[str] = []
    index = 0
    seen_ids: dict[str, int] = {}
    current_heading = page.title
    current_anchor = ""
    prose: list[str] = []

    def flush_search() -> None:
        nonlocal prose
        body = " ".join(prose).strip()
        if current_heading:
            page.search.append(
                {
                    "title": current_heading,
                    "page": page.title,
                    "url": f"{page.slug}.html" + (f"#{current_anchor}" if current_anchor else ""),
                    "text": body[:600],
                }
            )
        prose = []

    def anchor_for(text: str) -> str:
        base = slugify(text)
        if base in seen_ids:
            seen_ids[base] += 1
            return f"{base}-{seen_ids[base]}"
        seen_ids[base] = 0
        return base

    while index < len(lines):
        line = lines[index]
        stripped = line.strip()

        # fenced code
        if stripped.startswith("```"):
            lang = stripped[3:].strip() or "text"
            index += 1
            block: list[str] = []
            while index < len(lines) and not lines[index].strip().startswith("```"):
                block.append(lines[index])
                index += 1
            index += 1
            code = html.escape("\n".join(block), quote=False)
            out.append(
                '<figure class="code">'
                f'<figcaption><span class="lang">{html.escape(lang)}</span>'
                '<button class="copy" type="button" aria-label="Copy code">copy</button></figcaption>'
                f'<pre><code class="lang-{html.escape(lang)}">{code}</code></pre>'
                "</figure>"
            )
            continue

        # a live demo: the interactive site runs it, this one shows what it runs
        if stripped.startswith(":::demo"):
            spec = stripped[len(":::demo") :].strip()
            kind = spec.split()[0] if spec else "run"
            index += 1
            block = []
            while index < len(lines) and not lines[index].strip().startswith(":::"):
                block.append(lines[index])
                index += 1
            index += 1
            schema, query = split_demo("\n".join(block))
            out.append(demo_html(kind, schema, query))
            prose.append(plain(query))
            continue

        # callouts
        if stripped.startswith(":::"):
            head = stripped[3:].strip().split(None, 1)
            kind = head[0] if head else "note"
            label = head[1] if len(head) > 1 else kind.capitalize()
            index += 1
            block = []
            while index < len(lines) and not lines[index].strip().startswith(":::"):
                block.append(lines[index])
                index += 1
            index += 1
            inner = render_fragment("\n".join(block))
            out.append(
                f'<aside class="callout {html.escape(kind)}">'
                f'<p class="callout-label">{inline(label)}</p>{inner}</aside>'
            )
            prose.append(plain(label) + " " + plain(" ".join(block)))
            continue

        # headings
        match = re.match(r"(#{1,4})\s+(.*)", stripped)
        if match:
            level = len(match.group(1))
            text = match.group(2)
            if level == 1:
                index += 1
                continue  # the shell renders the page title
            flush_search()
            explicit = re.search(r"\s*\{#([A-Za-z0-9_-]+)\}\s*$", text)
            if explicit:
                text = text[: explicit.start()].rstrip()
            current_heading = plain(text)
            current_anchor = explicit.group(1) if explicit else anchor_for(text)
            if level in (2, 3):
                page.toc.append((level, current_anchor, current_heading))
            out.append(
                f'<h{level} id="{current_anchor}">{inline(text)}'
                f'<a class="anchor" href="#{current_anchor}" aria-label="Link to this section">#</a>'
                f"</h{level}>"
            )
            index += 1
            continue

        # raw HTML — a block of it, ended by a blank line
        if stripped.startswith("<") and not stripped.startswith("<="):
            block = []
            while index < len(lines) and lines[index].strip():
                block.append(lines[index])
                index += 1
            out.append("\n".join(block))
            continue

        # tables
        if stripped.startswith("|"):
            table: list[str] = []
            while index < len(lines) and lines[index].strip().startswith("|"):
                table.append(lines[index].strip())
                index += 1
            out.append(render_table(table))
            prose.append(" ".join(plain(row) for row in table))
            continue

        # blockquote
        if stripped.startswith(">"):
            quote: list[str] = []
            while index < len(lines) and lines[index].strip().startswith(">"):
                quote.append(re.sub(r"^\s*>\s?", "", lines[index]))
                index += 1
            out.append(f'<blockquote>{render_fragment(chr(10).join(quote))}</blockquote>')
            prose.append(plain(" ".join(quote)))
            continue

        # lists
        if re.match(r"[-*]\s+", stripped) or re.match(r"\d+[.)]\s+", stripped):
            block = []
            while index < len(lines) and lines[index].strip():
                block.append(lines[index])
                index += 1
            out.append(render_list(block))
            prose.append(plain(" ".join(block)))
            continue

        # rule
        if stripped in ("---", "***"):
            out.append("<hr>")
            index += 1
            continue

        if not stripped:
            index += 1
            continue

        # paragraph
        para = []
        while index < len(lines) and lines[index].strip() and not is_block_start(lines[index]):
            para.append(lines[index].strip())
            index += 1
        text = " ".join(para)
        out.append(f"<p>{inline(text)}</p>")
        prose.append(plain(text))

    flush_search()
    return "\n".join(out)


def is_block_start(line: str) -> bool:
    stripped = line.strip()
    return bool(
        stripped.startswith("```")
        or stripped.startswith(":::")
        or stripped.startswith("|")
        or (stripped.startswith("<") and not stripped.startswith("<="))
        or stripped.startswith(">")
        or re.match(r"#{1,4}\s", stripped)
        or re.match(r"[-*]\s+", stripped)
        or re.match(r"\d+[.)]\s+", stripped)
        or stripped in ("---", "***")
    )


DEMO_TITLES = {
    "lex": "the lexer, on this query",
    "parse": "the parse tree, on this query",
    "types": "what the typechecker makes of this query",
    "plan": "the plan this query compiles to",
    "run": "this query, one transition at a time",
    "store": "the rows this query reads, as stored bytes",
    "schema": "this schema, as the engine reads it",
}


def split_demo(body: str) -> tuple[str, str]:
    """A demo is a query, optionally preceded by a schema and a `---` line."""
    parts = re.split(r"^---\s*$", body, maxsplit=1, flags=re.M)
    if len(parts) == 2:
        return parts[0].strip(), parts[1].strip()
    return "", body.strip()


def demo_html(kind: str, schema: str, query: str) -> str:
    """The static stand-in: the same source, and where it comes alive.

    The interactive site runs these against the engine compiled to WebAssembly.
    A generated page cannot, so it shows what would be run rather than a claim
    about what the answer is — a screenshot of an answer is a thing that goes
    stale silently.
    """
    lang = "schema" if kind == "schema" else "sigla"
    source = f"{schema}\n\n{query}".strip() if schema else query
    code = html.escape(source, quote=False)
    said = DEMO_TITLES.get(kind, "this query, live")
    return (
        '<figure class="code demo">'
        f'<figcaption><span class="lang">{html.escape(lang)}</span>'
        f'<span class="demo-kind">{html.escape(said)}</span>'
        '<button class="copy" type="button" aria-label="Copy code">copy</button></figcaption>'
        f'<pre><code class="lang-{lang}">{code}</code></pre>'
        '</figure>'
    )


def render_fragment(source: str) -> str:
    """Render nested content (inside a callout or a quote) without touching the TOC."""
    scratch = Page(slug="", title="")
    return render(source, scratch)


def render_table(rows: list[str]) -> str:
    def cells(row: str) -> list[str]:
        row = row.strip()
        if row.startswith("|"):
            row = row[1:]
        if row.endswith("|"):
            row = row[:-1]
        # `\|` is a literal pipe inside a cell (union types are written with one).
        parts = re.split(r"(?<!\\)\|", row)
        return [cell.strip().replace("\\|", "|") for cell in parts]

    if len(rows) < 2:
        return ""
    header = cells(rows[0])
    body = [cells(row) for row in rows[2:]]
    head_html = "".join(f"<th>{inline(cell)}</th>" for cell in header)
    body_html = "".join(
        "<tr>" + "".join(f"<td>{inline(cell)}</td>" for cell in row) + "</tr>" for row in body
    )
    return (
        '<div class="table-wrap"><table><thead><tr>'
        + head_html
        + "</tr></thead><tbody>"
        + body_html
        + "</tbody></table></div>"
    )


def render_list(block: list[str]) -> str:
    """One list, with a single level of nesting (two-space indent)."""
    ordered = bool(re.match(r"\s*\d+[.)]\s+", block[0]))
    tag = "ol" if ordered else "ul"
    items: list[list[str]] = []
    nested: list[list[str] | None] = []

    for raw in block:
        indent = len(raw) - len(raw.lstrip())
        stripped = raw.strip()
        marker = re.match(r"([-*]|\d+[.)])\s+(.*)", stripped)
        if marker and indent < 2:
            items.append([marker.group(2)])
            nested.append(None)
        elif marker:
            if nested[-1] is None:
                nested[-1] = []
            nested[-1].append(marker.group(2))
        elif items:
            items[-1].append(stripped)

    out = [f"<{tag}>"]
    for item, sub in zip(items, nested):
        out.append("<li>" + inline(" ".join(item)))
        if sub:
            out.append("<ul>" + "".join(f"<li>{inline(entry)}</li>" for entry in sub) + "</ul>")
        out.append("</li>")
    out.append(f"</{tag}>")
    return "".join(out)


# --------------------------------------------------------------------------- #
# the shell
# --------------------------------------------------------------------------- #


def nav_html(active: str) -> str:
    parts = ['<nav class="nav" aria-label="Documentation">']
    for label, pages in NAV:
        parts.append(f'<p class="nav-group">{html.escape(label)}</p><ul>')
        for slug, title in pages:
            current = ' class="current" aria-current="page"' if slug == active else ""
            parts.append(f'<li><a href="{slug}.html"{current}>{html.escape(title)}</a></li>')
        parts.append("</ul>")
    parts.append("</nav>")
    return "".join(parts)


def toc_html(page: Page) -> str:
    if len(page.toc) < 2:
        return ""
    parts = ['<aside class="toc" aria-label="On this page"><p class="toc-label">On this page</p><ul>']
    for level, anchor, text in page.toc:
        parts.append(f'<li class="lvl{level}"><a href="#{anchor}">{html.escape(text)}</a></li>')
    parts.append("</ul></aside>")
    return "".join(parts)


def pager_html(slug: str, titles: dict[str, str]) -> str:
    position = ORDER.index(slug)
    previous = ORDER[position - 1] if position > 0 else None
    following = ORDER[position + 1] if position + 1 < len(ORDER) else None
    parts = ['<nav class="pager">']
    if previous:
        parts.append(
            f'<a class="prev" href="{previous}.html"><span>Previous</span>'
            f"{html.escape(titles[previous])}</a>"
        )
    else:
        parts.append("<span></span>")
    if following:
        parts.append(
            f'<a class="next" href="{following}.html"><span>Next</span>'
            f"{html.escape(titles[following])}</a>"
        )
    parts.append("</nav>")
    return "".join(parts)


SHELL = """<!doctype html>
<html lang="en" data-page="{slug}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{page_title}</title>
<meta name="description" content="{description}">
<link rel="icon" href="assets/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="topbar">
  <button class="menu" type="button" aria-label="Menu" aria-expanded="false">☰</button>
  <a class="brand" href="index.html"><span class="mark" aria-hidden="true"></span>
    <span class="brand-text"><b>{site}</b><i>{tagline}</i></span></a>
  <button class="search-open" type="button">
    <span>Search</span><kbd>/</kbd>
  </button>
  <button class="theme" type="button" aria-label="Toggle colour scheme">◐</button>
</header>
<div class="layout">
  <div class="sidebar">{nav}</div>
  <main id="main">
    <article class="prose">
      <p class="eyebrow">{group}</p>
      <h1>{title}</h1>
      {lede}
      {body}
      {pager}
    </article>
  </main>
  {toc}
</div>
<div class="search-modal" hidden>
  <div class="search-panel" role="dialog" aria-modal="true" aria-label="Search the documentation">
    <input type="search" placeholder="Search the documentation…" autocomplete="off" spellcheck="false">
    <ul class="results"></ul>
    <p class="search-hint">Enter opens · Esc closes · ↑↓ moves</p>
  </div>
</div>
<script src="assets/app.js"></script>
</body>
</html>
"""


def build() -> int:
    if not CONTENT.exists():
        print(f"no content directory at {CONTENT}", file=sys.stderr)
        return 1

    strict = "--strict" in sys.argv[1:]
    warnings = 0

    pages: dict[str, Page] = {}
    for slug in ORDER:
        path = CONTENT / f"{slug}.md"
        if not path.exists():
            print(f"warning: {path.name} is in the nav and missing from content/", file=sys.stderr)
            warnings += 1
            continue
        meta, source = parse_front_matter(path.read_text(encoding="utf-8"))
        page = Page(
            slug=slug,
            title=meta.get("title", slug.replace("-", " ").capitalize()),
            description=meta.get("description", ""),
        )
        page.body = render(source, page)
        pages[slug] = page

    stray = sorted(p.stem for p in CONTENT.glob("*.md") if p.stem not in ORDER)
    for name in stray:
        print(f"warning: content/{name}.md is not in the nav — not built", file=sys.stderr)
        warnings += 1

    titles = {slug: page.title for slug, page in pages.items()}
    group_of = {slug: label for label, entries in NAV for slug, _ in entries}

    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)
    shutil.copytree(ASSETS, OUT / "assets")

    index: list[dict] = []
    for slug, page in pages.items():
        lede = f'<p class="lede">{inline(page.description)}</p>' if page.description else ""
        (OUT / f"{slug}.html").write_text(
            SHELL.format(
                slug=slug,
                page_title=html.escape(
                    page.title if page.title == SITE_TITLE else f"{page.title} · {SITE_TITLE}"
                ),
                site=html.escape(SITE_TITLE),
                tagline=html.escape(SITE_TAGLINE),
                title=html.escape(page.title),
                description=html.escape(plain(page.description), quote=True),
                group=html.escape(group_of.get(slug, "")),
                nav=nav_html(slug),
                toc=toc_html(page),
                lede=lede,
                body=page.body,
                pager=pager_html(slug, titles),
            ),
            encoding="utf-8",
        )
        index.extend(page.search)

    (OUT / "search-index.json").write_text(json.dumps(index, separators=(",", ":")), encoding="utf-8")
    print(f"built {len(pages)} pages and {len(index)} search entries into {OUT.relative_to(ROOT)}/")
    if strict and warnings:
        print(f"--strict: {warnings} warning(s) are an error", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(build())
