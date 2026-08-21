/**
 * **The book, as this site reads it.**
 *
 * The pages are the same files the generated site builds from — imported raw,
 * parsed here, and never copied: two copies of a page is one page that goes
 * stale. The reading order comes from `website/nav.json`, which the generator
 * reads too, so the sidebar cannot disagree with the one it publishes.
 *
 * Parsing is per page and memoised. The search index is every page's headings,
 * which is every page parsed — so it is built when somebody first searches
 * rather than on the way to the first paragraph anyone reads.
 */
import navigation from '../../../website/nav.json'
import { frontMatter, render, type Entry, type Rendered } from './markdown'

const SOURCES = import.meta.glob('../../../website/content/*.md', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>

export type Page = {
  slug: string
  title: string
  description: string
  group: string
  source: string
}

/** The playground is this site's own page: interactive, so the book has none. */
export const PLAYGROUND = { slug: 'playground', title: 'Playground' }

export const GROUPS: { label: string; pages: { slug: string; title: string }[] }[] = [
  { label: 'Try it', pages: [PLAYGROUND] },
  ...navigation.groups,
]

export const ORDER: string[] = GROUPS.flatMap((group) => group.pages.map((page) => page.slug))

const GROUP_OF = new Map(
  GROUPS.flatMap((group) => group.pages.map((page) => [page.slug, group.label] as const)),
)

const TITLE_OF = new Map(
  GROUPS.flatMap((group) => group.pages.map((page) => [page.slug, page.title] as const)),
)

const PAGES = new Map<string, Page>()
for (const [path, source] of Object.entries(SOURCES)) {
  const slug = path.slice(path.lastIndexOf('/') + 1, -'.md'.length)
  const { meta, body } = frontMatter(source)
  PAGES.set(slug, {
    slug,
    title: meta.title ?? slug,
    description: meta.description ?? '',
    group: GROUP_OF.get(slug) ?? '',
    source: body,
  })
}

export function page(slug: string): Page | null {
  return PAGES.get(slug) ?? null
}

export function navTitle(slug: string): string {
  return TITLE_OF.get(slug) ?? PAGES.get(slug)?.title ?? slug
}

const RENDERED = new Map<string, Rendered>()

export function rendered(slug: string): Rendered | null {
  const found = RENDERED.get(slug)
  if (found) return found
  const source = PAGES.get(slug)
  if (!source) return null
  const result = render(source.source, source)
  RENDERED.set(slug, result)
  return result
}

let index: Entry[] | null = null

export function searchIndex(): Entry[] {
  index ??= ORDER.flatMap((slug) => rendered(slug)?.search ?? [])
  return index
}

export function neighbours(slug: string): { previous: string | null; next: string | null } {
  const at = ORDER.indexOf(slug)
  if (at === -1) return { previous: null, next: null }
  return {
    previous: at > 0 ? ORDER[at - 1] : null,
    next: at + 1 < ORDER.length ? ORDER[at + 1] : null,
  }
}
