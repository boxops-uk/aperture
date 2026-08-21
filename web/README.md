# The interactive site

The design book with the engine itself running in it. React and Vite, with
`fjord-engine` compiled to WebAssembly — so a page that shows what the lexer
does is *asking the lexer*, not paraphrasing it.

**The pages are `website/content/`**, imported raw and parsed here rather than
copied: one book, two renderers. The generated site in
[`website/`](../website/README.md) still builds and still publishes, and its
reading order — `website/nav.json` — is the one this sidebar renders. What this
site adds is that the demos are the engine: a `:::demo` block in the content is a
running lexer, parser, typechecker, planner, executor or database table, editable
in the page.

There is also a **workbench** at `/playground`: every view of one query at once,
which is the thing a paragraph cannot hold. A demo hands its query to it through
the URL.

```bash
../scripts/build-wasm.sh   # or: npm run wasm
npm install
npm run dev                # http://localhost:5173
npm run smoke              # builds, serves, and drives it in a real browser
```

`npm run wasm` writes `src/wasm/`, which is **not** checked in — a binary in git
is a binary somebody has to trust, and the build is one command. Without it the
page says so and points at the script; it does not fall back to a highlighter
written in JavaScript, because that is the thing being replaced.

## Demos, in the content

A demo is a fenced block in the Markdown, so it lives where the paragraph that
needs it lives:

```markdown
:::demo plan
N where code.Decl {file = F, name = N, line = _}; F = code.File P; P = "src/u"..
:::
```

Kinds: `lex` · `parse` · `types` · `plan` · `run` · `store` · `schema`. The body
is a query; a `---` line above it makes the part before it the schema the query
is written against, for a demo that is about compiling rather than running:

```markdown
:::demo plan
schema test { predicate Foo : { id : int, name : string } -> string }
---
X where test.Foo {id = 1, name = X}
:::
```

With no schema of its own a demo uses `schemas/demo.sigla`, which is the only one
with a database behind it — `run` and `store` demos need rows, so they use it.
`website/build.py` understands the same block and renders the source with a note
that it is live here, so the generated site stays honest rather than showing an
answer that would go stale.

## What is where

| Path | Holds |
|---|---|
| `src/App.tsx` | the router: a path is a page, and `/playground` is the workbench |
| `src/book/markdown.ts` | the book's dialect, as `website/build.py` renders it — prose out as HTML, code and demos out as components |
| `src/book/content.ts` | the pages, globbed raw from `website/content/`, parsed once each; the search index is every page's headings, built when somebody first searches |
| `src/book/Layout.tsx` | the shell: the bar, the reading order, the page, and one click listener so a link in the prose is a navigation |
| `src/book/Code.tsx` | a fenced block — painted by the engine's own lexer once a demo on the page has brought the module in, and by the fallback rules until then |
| `src/book/highlight.ts` | those fallback rules, ported from `website/assets/app.js`, for the languages the engine has no opinion about |
| `src/book/Search.tsx`, `Toc.tsx`, `router.ts`, `theme.ts` | search over the headings, the on-page contents, routing, and the light/dark choice |
| `src/demo/Demo.tsx` | a demo: the engine, the view the block asked for, and an editor over the query |
| `src/engine.ts` | the module, loaded once and shared — *demanded* by a demo, merely *observed* by everything else, so a page of prose does not fetch a compiler |
| `src/wasm.ts` | loading the module, and the TypeScript shape of the JSON it answers |
| `src/Playground.tsx` | the workbench: the source, the samples, the accordion of views, and the database beside them |
| `src/SchemaPane.tsx` | the schema, as text — the only form a browser can hold one in, since `import` resolution reads files — painted by the schema language's own lexer |
| `src/LoweredView.tsx` | the lowered tree as the query's own shape: a head, then one section per statement |
| `src/PlanPane.tsx` | the plan: each step's text is the engine's own (`print::steps`), with the structure a reader counts around it |
| `src/RunPane.tsx` | the debugger: the machine's state folded from the changes each step carries |
| `src/Transport.tsx` | the controls that move a run — any navigation pauses it, and the buttons keep their size as they change state |
| `src/DataTable.tsx` | the database, with the scan's range shaded across it — the panel the plan's numbers are about |
| `src/run.ts` | one fold, read by both panels, so they cannot disagree about which row the machine is on |
| `src/playback.ts` | play — one transition every fifth of a second, which is about as fast as a register can be read |
| `src/Editor.tsx` | a textarea with the real tokens painted underneath it — used for the query and the schema, since the only difference is which lexer produced the tokens |
| `src/TokenTable.tsx`, `src/TreeView.tsx` | the two views — the second walks the arena from its root, which is already in reading order |
| `src/span.ts` | what the cursor is on, and the rule every view highlights by: a node lights up **its subtree** and the bytes it covers, never the path above it — that is what the indentation already shows |
| `src/book.css`, `src/app.css`, `src/index.css` | the book, the workbench, and the palette they share |
| `smoke.mjs` | the end-to-end check — it drives the built bundle in Chrome, over both halves |

The token colours are keyed on `TokenClass`, which the Rust side decides
(`fjord_inspect::tokens`). A page styles what the language says a token *is*; it
never re-decides it. Adding a token to sigla therefore reaches the browser
without anyone editing a regex here — which is the whole argument for compiling
the engine rather than reimplementing it. The same holds for the tree: a rule
added to `grammar.llw` does not compile until `fjord-inspect` names it.

The sample queries, the schema and the facts all come from the **module**, not
from here: `fjord_inspect::SAMPLES` over `schemas/demo.sigla` and the demo
database, with `every_sample_compiles_clean` and `every_sample_answers_what_it_says`
asserting each one in the Rust suite — down to how many rows it answers, because
a demo query that returns nothing demonstrates nothing. That is not tidiness. The first version of this page invented
its own samples, and **every one of them was missing the head a query requires**
— the lexer tokenised them happily, and it took the parse view to notice.

## Two renderers, one book

The smoke check compares them page for page — headings, tables, code blocks,
callouts and demos — against `website/site/`, when that has been built. A dialect
that drifts is a page that reads differently depending on which copy of the site
you found, and the two parsers are in different languages, so nothing but a check
keeps them together.

## Serving it

A page is a path, so a host has to answer every path with the application:
`npm run build` writes `dist/404.html` as a copy of `dist/index.html`, which is
how GitHub Pages does that. `SITE_BASE=/fjord/ npm run build` sets the base for a
site served from a subdirectory.
