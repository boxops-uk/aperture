/**
 * **The book's markdown dialect** — the same one `website/build.py` renders.
 *
 * The content in `website/content/` is the design book, and it is written once:
 * the generated site and this one read the same files, so anything this parser
 * does differently is drift a reader can see. The dialect is deliberately small
 * — headings, paragraphs, fenced code, one level of list nesting, pipe tables,
 * blockquotes, callouts, rules, raw HTML blocks, and the usual inline marks.
 *
 * What comes out is a **tree, not HTML**. The generator's job ends at a string;
 * this one hands the blocks to components, so a table is a `Table`, a callout is
 * a `Banner`, and a `:::demo` is a running engine. The one exception is a raw
 * HTML block, which the dialect allows and the book uses for the home page's
 * card grid — that arrives as the string it was written as.
 */

/** A live demo: a query, and the schema it is written against. */
export type Demo = { kind: string; schema: string; query: string }

export type Inline =
  | { kind: 'text'; text: string }
  | { kind: 'code'; text: string }
  | { kind: 'strong'; children: Inline[] }
  | { kind: 'em'; children: Inline[] }
  | { kind: 'del'; children: Inline[] }
  | { kind: 'link'; href: string; children: Inline[] }

export type Item = { children: Inline[]; nested: Inline[][] }

export type Block =
  | { kind: 'heading'; level: number; anchor: string; children: Inline[] }
  | { kind: 'para'; children: Inline[] }
  | { kind: 'list'; ordered: boolean; items: Item[] }
  | { kind: 'table'; head: Inline[][]; rows: Inline[][][] }
  | { kind: 'quote'; blocks: Block[] }
  | { kind: 'callout'; tone: string; label: Inline[]; blocks: Block[] }
  | { kind: 'rule' }
  | { kind: 'code'; lang: string; source: string }
  | { kind: 'demo'; demo: Demo }
  | { kind: 'html'; html: string }

export type Heading = { level: number; anchor: string; text: string }

/** One search entry per heading: the heading, and the prose under it. */
export type Entry = { title: string; page: string; slug: string; anchor: string; text: string }

export type Rendered = { blocks: Block[]; toc: Heading[]; search: Entry[] }

const CODE_SPAN = /`([^`]+)`/
const LINK = /\[([^\]]+)\]\(([^)\s]+)\)/
const BOLD = /\*\*(.+?)\*\*/
const ITALIC = /(?<![*\w])\*([^*\n]+)\*(?!\*)/
const STRIKE = /~~(.+?)~~/

/** Where a link in the content points once the site is one application. */
export function href(target: string): string {
  if (/^(https?:|mailto:|#)/.test(target)) return target
  const [page, anchor] = target.split('#')
  // The book links between pages as `storage.html#keys`, because that is what
  // the generated site serves. Here a page is a route.
  if (page.endsWith('.html')) {
    const slug = page.slice(0, -'.html'.length)
    return route(slug) + (anchor ? `#${anchor}` : '')
  }
  return target
}

/** A page's path under whatever base the site is served from. */
export function route(slug: string): string {
  const base = import.meta.env.BASE_URL
  return slug === 'index' ? base : `${base}${slug}`
}

const MARKS: {
  pattern: RegExp
  make: (match: RegExpExecArray, depth: number) => Inline
}[] = [
  { pattern: CODE_SPAN, make: (match) => ({ kind: 'code', text: match[1] }) },
  {
    pattern: LINK,
    make: (match, depth) => ({
      kind: 'link',
      href: href(match[2]),
      children: level(match[1], depth + 1),
    }),
  },
  { pattern: BOLD, make: (match, depth) => ({ kind: 'strong', children: level(match[1], depth + 1) }) },
  { pattern: ITALIC, make: (match, depth) => ({ kind: 'em', children: level(match[1], depth + 1) }) },
  { pattern: STRIKE, make: (match, depth) => ({ kind: 'del', children: level(match[1], depth + 1) }) },
]

/**
 * The inline marks, in the order the generator applies them.
 *
 * Code spans come first and are opaque: a mark inside one is not a mark, which
 * is the whole reason the generator lifts them out before it escapes anything.
 * The rest nest, so each level parses the text around its own match with the
 * levels below it.
 */
export function inlines(text: string): Inline[] {
  return level(text, 0)
}

function level(text: string, depth: number): Inline[] {
  if (depth >= MARKS.length) return text ? [{ kind: 'text', text }] : []
  const { pattern, make } = MARKS[depth]
  const out: Inline[] = []
  let rest = text
  for (;;) {
    const match = pattern.exec(rest)
    if (!match) break
    out.push(...level(rest.slice(0, match.index), depth + 1))
    out.push(make(match, depth))
    rest = rest.slice(match.index + match[0].length)
  }
  out.push(...level(rest, depth + 1))
  return out
}

/** The same text with every mark removed — for the search index and the TOC. */
export function plain(text: string): string {
  return text
    .replace(new RegExp(CODE_SPAN.source, 'g'), '$1')
    .replace(new RegExp(LINK.source, 'g'), '$1')
    .replace(new RegExp(BOLD.source, 'g'), '$1')
    .replace(new RegExp(ITALIC.source, 'g'), '$1')
    .replace(new RegExp(STRIKE.source, 'g'), '$1')
    .trim()
}

export function slugify(text: string): string {
  const stripped = plain(text)
    .toLowerCase()
    .replace(/[^a-z0-9\s-]/g, '')
  return stripped.replace(/[\s-]+/g, '-').replace(/^-+|-+$/g, '') || 'section'
}

export function frontMatter(text: string): { meta: Record<string, string>; body: string } {
  if (!text.startsWith('---\n')) return { meta: {}, body: text }
  const end = text.indexOf('\n---\n', 4)
  if (end === -1) return { meta: {}, body: text }
  const meta: Record<string, string> = {}
  for (const line of text.slice(4, end).split('\n')) {
    const at = line.indexOf(':')
    if (at > 0) meta[line.slice(0, at).trim()] = line.slice(at + 1).trim()
  }
  return { meta, body: text.slice(end + 5) }
}

/** A demo is a query, optionally preceded by a schema and a `---` line. */
export function splitDemo(body: string): { schema: string; query: string } {
  const parts = body.split(/^---[ \t]*$/m)
  if (parts.length >= 2) return { schema: parts[0].trim(), query: parts.slice(1).join('---').trim() }
  return { schema: '', query: body.trim() }
}

const HEADING = /^(#{1,4})\s+(.*)/
const LIST_ITEM = /^([-*]|\d+[.)])\s+/
const EXPLICIT_ANCHOR = /\s*\{#([A-Za-z0-9_-]+)\}\s*$/

function isBlockStart(line: string): boolean {
  const stripped = line.trim()
  return (
    stripped.startsWith('```') ||
    stripped.startsWith(':::') ||
    stripped.startsWith('|') ||
    (stripped.startsWith('<') && !stripped.startsWith('<=')) ||
    stripped.startsWith('>') ||
    HEADING.test(stripped) ||
    LIST_ITEM.test(stripped) ||
    stripped === '---' ||
    stripped === '***'
  )
}

export function render(source: string, page: { slug: string; title: string }): Rendered {
  const lines = source.split('\n')
  const blocks: Block[] = []
  const toc: Heading[] = []
  const search: Entry[] = []
  const seen = new Map<string, number>()
  let index = 0
  let heading = page.title
  let anchor = ''
  let prose: string[] = []

  const flushSearch = () => {
    const text = prose.join(' ').trim()
    if (heading)
      search.push({
        title: heading,
        page: page.title,
        slug: page.slug,
        anchor,
        text: text.slice(0, 600),
      })
    prose = []
  }

  const anchorFor = (text: string) => {
    const base = slugify(text)
    const count = seen.get(base)
    if (count === undefined) {
      seen.set(base, 0)
      return base
    }
    seen.set(base, count + 1)
    return `${base}-${count + 1}`
  }

  while (index < lines.length) {
    const line = lines[index]
    const stripped = line.trim()

    // fenced code
    if (stripped.startsWith('```')) {
      const lang = stripped.slice(3).trim() || 'text'
      index++
      const block: string[] = []
      while (index < lines.length && !lines[index].trim().startsWith('```')) block.push(lines[index++])
      index++
      blocks.push({ kind: 'code', lang, source: block.join('\n') })
      continue
    }

    // a live demo
    if (stripped.startsWith(':::demo')) {
      const spec = stripped.slice(':::demo'.length).trim()
      const kind = spec.split(/\s+/)[0] || 'run'
      index++
      const block: string[] = []
      while (index < lines.length && !lines[index].trim().startsWith(':::')) block.push(lines[index++])
      index++
      const { schema, query } = splitDemo(block.join('\n'))
      blocks.push({ kind: 'demo', demo: { kind, schema, query } })
      prose.push(plain(query))
      continue
    }

    // callouts
    if (stripped.startsWith(':::')) {
      const head = stripped.slice(3).trim().split(/\s+(.*)/)
      const tone = head[0] || 'note'
      const label = head[1] ?? tone.charAt(0).toUpperCase() + tone.slice(1)
      index++
      const block: string[] = []
      while (index < lines.length && !lines[index].trim().startsWith(':::')) block.push(lines[index++])
      index++
      blocks.push({ kind: 'callout', tone, label: inlines(label), blocks: fragment(block.join('\n')) })
      prose.push(`${plain(label)} ${plain(block.join(' '))}`)
      continue
    }

    // headings
    const match = HEADING.exec(stripped)
    if (match) {
      const level = match[1].length
      let text = match[2]
      if (level === 1) {
        index++
        continue // the layout renders the page title
      }
      flushSearch()
      const explicit = EXPLICIT_ANCHOR.exec(text)
      if (explicit) text = text.slice(0, explicit.index).trimEnd()
      heading = plain(text)
      anchor = explicit ? explicit[1] : anchorFor(text)
      if (level === 2 || level === 3) toc.push({ level, anchor, text: heading })
      blocks.push({ kind: 'heading', level, anchor, children: inlines(text) })
      index++
      continue
    }

    // raw HTML — a block of it, ended by a blank line
    if (stripped.startsWith('<') && !stripped.startsWith('<=')) {
      const block: string[] = []
      while (index < lines.length && lines[index].trim()) block.push(lines[index++])
      blocks.push({ kind: 'html', html: rewriteLinks(block.join('\n')) })
      continue
    }

    // tables
    if (stripped.startsWith('|')) {
      const table: string[] = []
      while (index < lines.length && lines[index].trim().startsWith('|')) table.push(lines[index++].trim())
      const parsed = renderTable(table)
      if (parsed) blocks.push(parsed)
      prose.push(table.map(plain).join(' '))
      continue
    }

    // blockquote
    if (stripped.startsWith('>')) {
      const quote: string[] = []
      while (index < lines.length && lines[index].trim().startsWith('>'))
        quote.push(lines[index++].replace(/^\s*>\s?/, ''))
      blocks.push({ kind: 'quote', blocks: fragment(quote.join('\n')) })
      prose.push(plain(quote.join(' ')))
      continue
    }

    // lists
    if (LIST_ITEM.test(stripped)) {
      const block: string[] = []
      while (index < lines.length && lines[index].trim()) block.push(lines[index++])
      blocks.push(renderList(block))
      prose.push(plain(block.join(' ')))
      continue
    }

    // rule
    if (stripped === '---' || stripped === '***') {
      blocks.push({ kind: 'rule' })
      index++
      continue
    }

    if (!stripped) {
      index++
      continue
    }

    // paragraph
    const para: string[] = []
    while (index < lines.length && lines[index].trim() && !isBlockStart(lines[index]))
      para.push(lines[index++].trim())
    const text = para.join(' ')
    blocks.push({ kind: 'para', children: inlines(text) })
    prose.push(plain(text))
  }

  flushSearch()
  return { blocks, toc, search }
}

/** Nested content — inside a callout or a quote — without touching the TOC. */
function fragment(source: string): Block[] {
  return render(source, { slug: '', title: '' }).blocks
}

/** `href="x.html"` inside a raw HTML block is a link between pages too. */
function rewriteLinks(html: string): string {
  return html.replace(/href="([^"]+)"/g, (_, target: string) => `href="${href(target)}"`)
}

function renderTable(rows: string[]): Block | null {
  const cells = (row: string): Inline[][] => {
    let text = row.trim()
    if (text.startsWith('|')) text = text.slice(1)
    if (text.endsWith('|')) text = text.slice(0, -1)
    // `\|` is a literal pipe inside a cell (union types are written with one).
    return text.split(/(?<!\\)\|/).map((cell) => inlines(cell.trim().replace(/\\\|/g, '|')))
  }

  if (rows.length < 2) return null
  return { kind: 'table', head: cells(rows[0]), rows: rows.slice(2).map(cells) }
}

function renderList(block: string[]): Block {
  const ordered = /^\s*\d+[.)]\s+/.test(block[0])
  const items: string[][] = []
  const nested: (string[] | null)[] = []

  for (const raw of block) {
    const indent = raw.length - raw.trimStart().length
    const stripped = raw.trim()
    const marker = /^([-*]|\d+[.)])\s+(.*)/.exec(stripped)
    if (marker && indent < 2) {
      items.push([marker[2]])
      nested.push(null)
    } else if (marker) {
      if (nested[nested.length - 1] === null) nested[nested.length - 1] = []
      nested[nested.length - 1]?.push(marker[2])
    } else if (items.length) {
      items[items.length - 1].push(stripped)
    }
  }

  return {
    kind: 'list',
    ordered,
    items: items.map((item, at) => ({
      children: inlines(item.join(' ')),
      nested: (nested[at] ?? []).map(inlines),
    })),
  }
}
