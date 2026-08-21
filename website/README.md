# The Fjord documentation site

A static documentation site for Fjord DB, plus a small server to preview it. Standard
library Python only — no toolchain, no dependencies, no network access at build time.

**This is not what publishes.** <https://boxops-uk.github.io/fjord/> is the interactive site
in [`web/`](../web/README.md) — the same pages, with the engine running in them — and
`fjord-docs-site.tar.gz` on a release is that bundle. What this generator is: the copy that
reads with **no toolchain and no network**, and the second renderer the interactive site's
smoke check compares itself against page for page. A dialect that drifts is a page that reads
differently depending on which copy you found, so it still builds strictly in CI.

```bash
python3 serve.py            # build, then serve on http://127.0.0.1:8000
python3 serve.py --watch    # …and rebuild whenever content/ or assets/ changes
python3 build.py            # build only, into site/
python3 build.py --strict   # what CI runs: the generator's warnings become an error
```

Other flags: `--port N`, `--host 0.0.0.0` (to reach it from another machine), `--no-build`
(serve `site/` exactly as it is).

## Layout

```text
website/
├── build.py        the generator: content/*.md → site/*.html + search-index.json
├── nav.json        the reading order — read by build.py and by web/
├── serve.py        the preview server (static, no-cache, optional --watch)
├── content/        one Markdown file per page — this is what you edit
├── assets/         style.css, app.js, favicon.svg — copied through verbatim
└── site/           the build output. Generated; safe to delete
```

## Editing

Each page is `content/<slug>.md` with a small front matter block:

```markdown
---
title: Page title
description: One sentence, shown as the standfirst and used for the search index.
---
```

**The navigation is the reading order**, and it lives in [`nav.json`](nav.json) — read by this
generator *and* by the interactive site in [`web/`](../web/README.md), so the two cannot
publish the pages in different orders. A page is built only if its slug is in `nav.json`; a
slug there with no file, or a file with no entry, is reported as a warning at build time.

### The Markdown dialect

Deliberately small — headings, paragraphs, fenced code, lists (one level of nesting), pipe
tables (`\|` for a literal pipe in a cell), blockquotes, horizontal rules, and the usual inline marks (`**bold**`, `*italic*`,
`` `code` ``, `[links](x.html)`). Plus two extras:

```markdown
:::note Optional label
A callout. Kinds: note · warn · invariant · gap.
:::

### A heading with an explicit anchor {#i5}
```

And one that only the interactive site can honour:

```markdown
:::demo plan
N where code.Decl {file = F, name = N, line = _}; F = code.File P; P = "src/u"..
:::
```

A **demo** is the engine itself, running in the page — kinds `lex`, `parse`, `types`, `plan`,
`run`, `store` and `schema`, with an optional schema above a `---` line. This generator has no
engine to run, so it renders the source and says where it comes alive; `web/` renders the
engine. Either way the page has no typed-out answer in it to go stale.

A block starting with `<` at the beginning of a line is passed through as raw HTML until the
next blank line — which is how the home page's card grid and the status pills are written.

A pipe character inside a table cell ends the cell, so write "a second source" rather than
`| src.Foo` in table prose.

### Code fences

The language tag drives a small client-side highlighter in `assets/app.js`:

`sigla` · `schema` (the schema DSL) · `plan` (the `:plan` renderer's output) · `rust` · `csharp` ·
`python` · `bash` · `json` · `text` (no highlighting).

The highlighter is lossless — it only wraps spans — so a code block always shows exactly what
was written.

## Features

- Sidebar navigation grouped by reading order, with the current page marked
- On-page table of contents that tracks the scroll position
- Client-side search over every heading and its prose (`/` or ⌘K, arrows, Enter)
- Light and dark themes: follows the system by default, with a toggle that sticks
- Copy buttons on every code block, anchor links on every heading, prev/next pagers
- Responsive down to a phone, and a print stylesheet
- No external requests of any kind: no CDN, no webfont, no analytics

## Where the content comes from

The pages are written from the design record in the repository — the design book in `docs/`,
the invariant registry, `PLAN.md`, and `bench/FINDINGS.md` — and every command, output block
and diagnostic quoted in *Getting started* and the *Walkthrough* was run against the built
binaries rather than written from memory.

When the code changes, the pages most likely to drift are, in order:
`cli.md` (flags), `status.md` (what is built), `performance.md` (numbers),
`query-language.md` (diagnostics) and `wire-protocol.md` (frame kinds).
