/**
 * **The fallback painter** — the same rules the generated site uses.
 *
 * `sigla`, `schema` and `plan` blocks are painted by the engine's own lexer as
 * soon as the module has loaded (see `Code`), which is the whole point of a
 * site that carries the engine. These rules cover the languages the engine has
 * no opinion about — Rust, C#, Python, shells, JSON — and stand in for the
 * others until the module lands, so a page is never a wall of grey.
 *
 * Lossless by construction: it only wraps spans, so a block always shows
 * exactly what was written.
 */
import { escapeHtml } from './markdown'

type Rule = [string, RegExp]

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
  python: [
    ['com', /#[^\n]*/],
    ['str', /"""[\s\S]*?"""|"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'/],
    [
      'kw',
      /\b(?:and|as|assert|class|def|elif|else|except|finally|for|from|if|import|in|is|lambda|none|not|or|pass|raise|return|try|while|with|yield|None|True|False)\b/,
    ],
    ['num', /\b\d[\d_]*(?:\.\d+)?\b/],
    ['fn', /\b[A-Z][A-Za-z0-9_]*\b/],
    ['pun', /[{}()[\]:;,.=!+\-*|?]/],
  ],
  bash: [
    ['com', /#[^\n]*/],
    ['str', /"(?:[^"\\\n]|\\.)*"|'[^'\n]*'/],
    [
      'kw',
      /\b(?:cargo|python3|dotnet|fjord|fjord-viewer|git|export|cd|rm|mkdir|sleep|while|do|done|if|then|fi|kill|tar|curl|echo)\b/,
    ],
    ['num', /\b\d+\b/],
    ['fn', /(?:^|\s)--?[A-Za-z][\w-]*/],
    ['pun', /[|&;<>(){}$]/],
  ],
  json: [
    ['str', /"(?:[^"\\\n]|\\.)*"/],
    ['kw', /\b(?:true|false|null)\b/],
    ['num', /-?\b\d+(?:\.\d+)?\b/],
    ['pun', /[{}[\]:,]/],
  ],
}

RULES.sh = RULES.bash
RULES.console = RULES.bash
RULES.jsonl = RULES.json
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

export function paint(source: string, language: string): string {
  const rules = COMPILED.get(language)
  if (!rules) return escapeHtml(source)

  let out = ''
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
    if (bestIndex === -1 || bestMatch === null) {
      out += escapeHtml(source.slice(at))
      break
    }
    out += escapeHtml(source.slice(at, bestIndex))
    const lead = /^\s+/.exec(bestMatch)
    if (lead) {
      out += escapeHtml(lead[0])
      bestMatch = bestMatch.slice(lead[0].length)
    }
    out += `<span class="tok-${bestKind}">${escapeHtml(bestMatch)}</span>`
    at = bestIndex + (lead ? lead[0].length : 0) + bestMatch.length
  }
  return out
}
