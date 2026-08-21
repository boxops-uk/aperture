/**
 * **The book's markdown dialect** — the same one `website/build.py` renders.
 *
 * The content in `website/content/` is the design book, and it is written once:
 * the generated site and this one read the same files, so anything this parser
 * does differently is drift a reader can see. The dialect is deliberately small
 * — headings, paragraphs, fenced code, one level of list nesting, pipe tables,
 * blockquotes, callouts, rules, raw HTML blocks, and the usual inline marks.
 *
 * Two things come out as *blocks* rather than HTML, because they are React: a
 * fenced code block, which paints itself with the engine's own lexer once the
 * module has loaded, and a `:::demo`, which is a running engine. Everything
 * else is a string of HTML, which is what it was in the first place.
 */

/** A live demo: a query, and the schema it is written against. */
export type Demo = { kind: string; schema: string; query: string }

export type Block =
  | { kind: 'html'; html: string }
  | { kind: 'code'; lang: string; source: string }
  | { kind: 'demo'; demo: Demo }

export type Heading = { level: number; anchor: string; text: string }

/** One search entry per heading: the heading, and the prose under it. */
export type Entry = { title: string; page: string; slug: string; anchor: string; text: string }

export type Rendered = { blocks: Block[]; toc: Heading[]; search: Entry[] }

const CODE_SPAN = /`([^`]+)`/g
const LINK = /\[([^\]]+)\]\(([^)\s]+)\)/g
const BOLD = /\*\*(.+?)\*\*/g
const ITALIC = /(?<![*\w])\*([^*\n]+)\*(?!\*)/g
const STRIKE = /~~(.+?)~~/g

export function escapeHtml(text: string): string {
  return text.replace(/[&<>]/g, (c) => (c === '&' ? '&amp;' : c === '<' ? '&lt;' : '&gt;'))
}

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

export function inline(text: string): string {
  // Code spans are lifted out before escaping, so a `<` inside one is escaped
  // exactly once and a mark inside one is not a mark.
  const spans: string[] = []
  let out = text.replace(CODE_SPAN, (_, code: string) => {
    spans.push(escapeHtml(code))
    // A private-use codepoint: prose cannot contain one, and a bare number
    // between two spaces is text that occurs constantly.
    return `\uE000${spans.length - 1}\uE000`
  })
  out = escapeHtml(out)
  out = out.replace(LINK, (_, label: string, target: string) => {
    const to = href(target)
    const external = /^https?:/.test(to)
    return `<a href="${to.replace(/"/g, '&quot;')}"${external ? ' rel="noreferrer"' : ''}>${label}</a>`
  })
  out = out.replace(BOLD, '<strong>$1</strong>')
  out = out.replace(ITALIC, '<em>$1</em>')
  out = out.replace(STRIKE, '<del>$1</del>')
  return out.replace(/\uE000(\d+)\uE000/g, (_, index: string) => `<code>${spans[Number(index)]}</code>`)
}

/** The same text with every mark removed — for the search index and the TOC. */
export function plain(text: string): string {
  return text
    .replace(CODE_SPAN, '$1')
    .replace(LINK, '$1')
    .replace(BOLD, '$1')
    .replace(ITALIC, '$1')
    .replace(STRIKE, '$1')
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

type Sink = { blocks: Block[]; html: string[] }

function flushHtml(sink: Sink): void {
  if (sink.html.length === 0) return
  sink.blocks.push({ kind: 'html', html: sink.html.join('\n') })
  sink.html = []
}

export function render(source: string, page: { slug: string; title: string }): Rendered {
  const lines = source.split('\n')
  const sink: Sink = { blocks: [], html: [] }
  const toc: Heading[] = []
  const search: Entry[] = []
  const seen = new Map<string, number>()
  let index = 0
  let heading = page.title
  let anchor = ''
  let prose: string[] = []

  const flushSearch = () => {
    const text = prose.join(' ').trim()
    if (heading) search.push({ title: heading, page: page.title, slug: page.slug, anchor, text: text.slice(0, 600) })
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
      flushHtml(sink)
      sink.blocks.push({ kind: 'code', lang, source: block.join('\n') })
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
      flushHtml(sink)
      sink.blocks.push({ kind: 'demo', demo: { kind, schema, query } })
      prose.push(plain(query))
      continue
    }

    // callouts
    if (stripped.startsWith(':::')) {
      const head = stripped.slice(3).trim().split(/\s+(.*)/)
      const kind = head[0] || 'note'
      const label = head[1] ?? kind.charAt(0).toUpperCase() + kind.slice(1)
      index++
      const block: string[] = []
      while (index < lines.length && !lines[index].trim().startsWith(':::')) block.push(lines[index++])
      index++
      sink.html.push(
        `<aside class="callout ${escapeHtml(kind)}"><p class="callout-label">${inline(label)}</p>${fragment(block.join('\n'))}</aside>`,
      )
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
      sink.html.push(
        `<h${level} id="${anchor}">${inline(text)}` +
          `<a class="anchor" href="#${anchor}" aria-label="Link to this section">#</a></h${level}>`,
      )
      index++
      continue
    }

    // raw HTML — a block of it, ended by a blank line
    if (stripped.startsWith('<') && !stripped.startsWith('<=')) {
      const block: string[] = []
      while (index < lines.length && lines[index].trim()) block.push(lines[index++])
      sink.html.push(rewriteLinks(block.join('\n')))
      continue
    }

    // tables
    if (stripped.startsWith('|')) {
      const table: string[] = []
      while (index < lines.length && lines[index].trim().startsWith('|')) table.push(lines[index++].trim())
      sink.html.push(renderTable(table))
      prose.push(table.map(plain).join(' '))
      continue
    }

    // blockquote
    if (stripped.startsWith('>')) {
      const quote: string[] = []
      while (index < lines.length && lines[index].trim().startsWith('>'))
        quote.push(lines[index++].replace(/^\s*>\s?/, ''))
      sink.html.push(`<blockquote>${fragment(quote.join('\n'))}</blockquote>`)
      prose.push(plain(quote.join(' ')))
      continue
    }

    // lists
    if (LIST_ITEM.test(stripped)) {
      const block: string[] = []
      while (index < lines.length && lines[index].trim()) block.push(lines[index++])
      sink.html.push(renderList(block))
      prose.push(plain(block.join(' ')))
      continue
    }

    // rule
    if (stripped === '---' || stripped === '***') {
      sink.html.push('<hr>')
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
    sink.html.push(`<p>${inline(text)}</p>`)
    prose.push(plain(text))
  }

  flushSearch()
  flushHtml(sink)
  return { blocks: sink.blocks, toc, search }
}

/** Nested content — inside a callout or a quote — without touching the TOC. */
function fragment(source: string): string {
  const { blocks } = render(source, { slug: '', title: '' })
  return blocks
    .map((block) =>
      block.kind === 'html'
        ? block.html
        : block.kind === 'code'
          ? `<figure class="code"><figcaption><span class="lang">${escapeHtml(block.lang)}</span></figcaption><pre><code class="lang-${escapeHtml(block.lang)}">${escapeHtml(block.source)}</code></pre></figure>`
          : '',
    )
    .join('\n')
}

/** `href="x.html"` inside a raw HTML block is a link between pages too. */
function rewriteLinks(html: string): string {
  return html.replace(/href="([^"]+)"/g, (_, target: string) => `href="${href(target)}"`)
}

function renderTable(rows: string[]): string {
  const cells = (row: string): string[] => {
    let text = row.trim()
    if (text.startsWith('|')) text = text.slice(1)
    if (text.endsWith('|')) text = text.slice(0, -1)
    // `\|` is a literal pipe inside a cell (union types are written with one).
    return text.split(/(?<!\\)\|/).map((cell) => cell.trim().replace(/\\\|/g, '|'))
  }

  if (rows.length < 2) return ''
  const head = cells(rows[0])
    .map((cell) => `<th>${inline(cell)}</th>`)
    .join('')
  const body = rows
    .slice(2)
    .map((row) => `<tr>${cells(row).map((cell) => `<td>${inline(cell)}</td>`).join('')}</tr>`)
    .join('')
  return `<div class="table-wrap"><table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`
}

function renderList(block: string[]): string {
  const ordered = /^\s*\d+[.)]\s+/.test(block[0])
  const tag = ordered ? 'ol' : 'ul'
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

  const out = [`<${tag}>`]
  items.forEach((item, at) => {
    out.push(`<li>${inline(item.join(' '))}`)
    const sub = nested[at]
    if (sub) out.push(`<ul>${sub.map((entry) => `<li>${inline(entry)}</li>`).join('')}</ul>`)
    out.push('</li>')
  })
  out.push(`</${tag}>`)
  return out.join('')
}
