# The interactive site

The design book with the engine itself running in it. React and Vite, with
`fjord-engine` compiled to WebAssembly — so a page that shows what the lexer
does is *asking the lexer*, not paraphrasing it.

It lives beside [`website/`](../website/README.md) rather than replacing it: the
book is published on every push to main and nothing here is finished enough to
stand in for it yet. Two segments exist — **tokens** and the **parse tree** —
and between them they cover the front end's first two phases. The lexer came
first because it is the one that retires something: `website/assets/app.js`
carries a hand-written sigla highlighter, which is a second implementation of
the lexer and a second thing to keep true.

Everything after parsing waits on a schema being in the page, because lowering
resolves names against one.

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

## What is where

| Path | Holds |
|---|---|
| `src/wasm.ts` | loading the module once, and the TypeScript shape of the JSON it answers |
| `src/App.tsx` | the shell: the source editor, the sample queries, and the tabbed view beside them |
| `src/Editor.tsx` | a textarea with the real tokens painted underneath it |
| `src/TokenTable.tsx`, `src/TreeView.tsx` | the two views — the second walks the arena from its root, which is already in reading order |
| `src/span.ts` | what the cursor is on, and the rule every view highlights by: a node lights up **its subtree** and the bytes it covers, never the path above it — that is what the indentation already shows |
| `src/app.css` | the design book's palette, so the two sites read as one |
| `smoke.mjs` | the end-to-end check — it drives the built bundle in Chrome and asserts the tokens are the lexer's |

The token colours are keyed on `TokenClass`, which the Rust side decides
(`fjord_inspect::tokens`). A page styles what the language says a token *is*; it
never re-decides it. Adding a token to sigla therefore reaches the browser
without anyone editing a regex here — which is the whole argument for compiling
the engine rather than reimplementing it. The same holds for the tree: a rule
added to `grammar.llw` does not compile until `fjord-inspect` names it.

The sample queries are lifted from `fjord_engine::corpus`, where each one is
already classified and its answer already asserted. That is not tidiness. The
first version of this page invented its own samples, and **every one of them
was missing the head a query requires** — the lexer tokenised them happily, and
it took the parse view to notice.
