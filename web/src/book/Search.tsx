import { useMemo } from 'react'
import { CommandPalette } from '@astryxdesign/core/CommandPalette'
import { Text } from '@astryxdesign/core/Text'
import { VStack } from '@astryxdesign/core/Stack'
import type { SearchSource, SearchableItem } from '@astryxdesign/core/Typeahead'
import { searchIndex } from './content'
import { route, type Entry } from './markdown'
import { navigate } from './router'

/**
 * Search over every heading and the prose beneath it — the same index the
 * generated site ships, built here from the same pages.
 *
 * Scored rather than filtered, and the scoring is deliberately blunt: a word in
 * a heading beats a word in a paragraph, a word missing everywhere sinks the
 * entry outright. Twenty pages do not need a ranking model; they need the
 * heading you were thinking of to be first.
 */
type Hit = SearchableItem<{ group: string; entry: Entry; needle: string }>

export function Search({
  isOpen,
  onOpenChange,
}: {
  isOpen: boolean
  onOpenChange: (open: boolean) => void
}) {
  const source: SearchSource<Hit> = useMemo(() => {
    const index = searchIndex()
    const hit = (entry: Entry, needle: string): Hit => ({
      id: `${entry.slug}#${entry.anchor}`,
      label: entry.title,
      auxiliaryData: { group: entry.page, entry, needle },
    })
    return {
      search: (query: string) => {
        const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean)
        if (words.length === 0) return []
        return index
          .map((entry) => ({ entry, score: score(entry, words) }))
          .filter((scored) => scored.score > 0)
          .sort((a, b) => b.score - a.score)
          .slice(0, 24)
          .map((scored) => hit(scored.entry, words[0]))
      },
      // What the palette shows before a key is pressed: where each page starts.
      bootstrap: () => index.filter((entry) => entry.anchor === '').map((entry) => hit(entry, '')),
    }
  }, [])

  return (
    <CommandPalette
      isOpen={isOpen}
      onOpenChange={onOpenChange}
      searchSource={source}
      label="Search the documentation"
      emptySearchText="Nothing matched."
      emptyBootstrapText="Type to search titles and body text."
      onValueChange={(value) => {
        const [slug, anchor] = value.split('#')
        navigate(route(slug) + (anchor ? `#${anchor}` : ''))
        onOpenChange(false)
      }}
      renderItem={(item) => {
        const { entry, needle } = item.auxiliaryData ?? { entry: null, needle: '' }
        if (!entry) return item.label
        return (
          <VStack gap={0}>
            <Text>{item.label}</Text>
            <Text type="supporting" maxLines={1}>
              {snippet(entry.text, needle)}
            </Text>
          </VStack>
        )
      }}
    />
  )
}

function score(entry: Entry, words: string[]): number {
  const title = entry.title.toLowerCase()
  const page = entry.page.toLowerCase()
  const text = entry.text.toLowerCase()
  let total = 0
  for (const word of words) {
    if (title.startsWith(word)) total += 60
    else if (title.includes(word)) total += 40
    if (page.includes(word)) total += 12
    if (text.includes(word)) total += 8
    if (!title.includes(word) && !text.includes(word) && !page.includes(word)) total -= 100
  }
  return total
}

/** The line the word was found on, rather than the first line of the section. */
function snippet(text: string, needle: string): string {
  if (!needle) return text.slice(0, 120)
  const at = text.toLowerCase().indexOf(needle)
  if (at < 0) return text.slice(0, 120)
  const from = Math.max(0, at - 45)
  return (from ? '…' : '') + text.slice(from, from + 150)
}
