/**
 * **The fallback tokenizer** — the same rules the generated site paints with.
 *
 * `CodeBlock` knows shells, JSON and the JavaScript family; it has never heard
 * of sigla, and it has no Rust. These rules cover the difference, and they are
 * the ones in `website/assets/app.js` so a block looks the same in both copies
 * of the book. A `sigla` or `schema` block only falls back to them until the
 * engine has loaded — after that the real lexer paints it.
 *
 * Lossless by construction: it emits spans, never text, so a block always shows
 * exactly what was written.
 */

/** What `CodeBlock` expects from a tokenizer: a type and a span. */
export type SyntaxToken = { type: string; start: number; end: number }

type Rule = [string, RegExp]

/** Our rule names, as the design system's syntax token keys. */
const AS: Record<string, string> = {
  kw: 'keyword',
  str: 'string',
  num: 'number',
  com: 'comment',
  fn: 'function',
  pun: 'punctuation',
  var: 'variable',
}

const RULES: Record<string, Rule[]> = {
  sigla: [
    ['com', /#[^\n]*/],
    ['str', /"(?:[^"\\\n]|\\.)*"/],
    ['kw', /\b(?:where|never)\b/],
    ['fn', /\b[a-z][A-Za-z0-9_]*(?:\.[a-z][A-Za-z0-9_]*)*\.[A-Z][A-Za-z0-9_]*/],
    ['num', /-?\b\d[\d_]*\b/],
    ['var', /\b[A-Z][A-Za-z0-9_]*\b/],
    ['pun', /[{}()=|!<>+\-;,?]+|\.\./],
  ],
  schema: [
    ['com', /#[^\n]*/],
    ['str', /"(?:[^"\\\n]|\\.)*"/],
    ['kw', /\b(?:schema|predicate|import|type|derive|stored|evolves|enum|maybe|set)\b/],
    ['fn', /\b(?:int|string)\b/],
    ['num', /\b\d[\d_]*\b/],
    ['var', /\b[A-Z][A-Za-z0-9_]*\b/],
    ['pun', /->|[{}()[\]:,|=]/],
  ],
  plan: [
    ['com', /#[^\n]*/],
    ['str', /"(?:[^"\\\n]|\\.)*"/],
    ['kw', /\b(?:scan|seek|fetch|absent|head|where|value)\b/],
    ['var', /\br\d+#?/],
    ['num', /-?\b\d[\d_]*\b/],
    ['fn', /\b[a-z][A-Za-z0-9_]*\.[A-Z][A-Za-z0-9_]*/],
    ['pun', /<-|==|!=|>=|<=|[{}()[\]=|+\-,.]/],
  ],
  rust: [
    ['com', /\/\/[^\n]*/],
    ['str', /"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)'/],
    [
      'kw',
      /\b(?:as|async|await|break|const|continue|crate|dyn|else|enum|fn|for|if|impl|in|let|loop|match|mod|move|mut|pub|ref|return|self|Self|static|struct|trait|type|unsafe|use|where|while)\b/,
    ],
    ['num', /\b\d[\d_]*(?:\.\d+)?(?:[iuf](?:8|16|32|64|size))?\b/],
    ['fn', /\b[A-Z][A-Za-z0-9_]*\b/],
    ['pun', /->|=>|::|[{}()[\]<>:;,.&*=!+\-|?]/],
  ],
  csharp: [
    ['com', /\/\/[^\n]*/],
    ['str', /"(?:[^"\\\n]|\\.)*"/],
    [
      'kw',
      /\b(?:async|await|class|const|else|for|foreach|if|in|internal|namespace|new|null|out|override|private|public|readonly|record|return|sealed|static|struct|this|throw|using|var|void|while)\b/,
    ],
    ['num', /\b\d[\d_]*\b/],
    ['fn', /\b[A-Z][A-Za-z0-9_]*\b/],
    ['pun', /=>|[{}()[\]<>:;,.=!+\-|?]/],
  ],
}

RULES.cs = RULES.csharp

// One global regex per rule, reused with `lastIndex` — recompiling per token
// would make a long block quadratic in rule count for no reason.
const COMPILED = new Map<string, Rule[]>(
  Object.entries(RULES).map(([language, rules]) => [
    language,
    rules.map(([kind, pattern]) => [kind, new RegExp(pattern.source, 'g')] as Rule),
  ]),
)

export function paints(language: string): boolean {
  return COMPILED.has(language)
}

export function tokenize(source: string, language: string): SyntaxToken[] {
  const rules = COMPILED.get(language)
  if (!rules) return []

  const out: SyntaxToken[] = []
  let at = 0
  while (at < source.length) {
    let bestIndex = -1
    let bestKind: string | null = null
    let bestMatch: string | null = null
    for (const [kind, pattern] of rules) {
      pattern.lastIndex = at
      const found = pattern.exec(source)
      if (found && (bestIndex === -1 || found.index < bestIndex)) {
        bestIndex = found.index
        bestKind = kind
        bestMatch = found[0]
      }
      if (bestIndex === at) break
    }
    if (bestIndex === -1 || bestMatch === null || bestKind === null) break
    const lead = /^\s+/.exec(bestMatch)
    const start = bestIndex + (lead ? lead[0].length : 0)
    const text = lead ? bestMatch.slice(lead[0].length) : bestMatch
    out.push({ type: AS[bestKind] ?? bestKind, start, end: start + text.length })
    at = start + text.length
  }
  return out
}
