# The interactive site

The design book with the engine itself running in it. React and Vite, with
`fjord-engine` compiled to WebAssembly — so a page that shows what the lexer
does is *asking the lexer*, not paraphrasing it.

It lives beside [`website/`](../website/README.md) rather than replacing it: the
book is published on every push to main and nothing here is finished enough to
stand in for it yet. The first segment is the lexer, because that is the one
that retires something — `website/assets/app.js` carries a hand-written sigla
highlighter, which is a second implementation of the lexer and a second thing to
keep true.

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
| `src/App.tsx` | the lexer segment: a textarea with the real tokens painted under it, and the token table beside it |
| `src/app.css` | the design book's palette, so the two sites read as one |
| `smoke.mjs` | the end-to-end check — it drives the built bundle in Chrome and asserts the tokens are the lexer's |

The token colours are keyed on `TokenClass`, which the Rust side decides
(`fjord_inspect::tokens`). A page styles what the language says a token *is*; it
never re-decides it. Adding a token to sigla therefore reaches the browser
without anyone editing a regex here — which is the whole argument for compiling
the engine rather than reimplementing it.
