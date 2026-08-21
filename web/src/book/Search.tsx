import { useEffect, useMemo, useRef, useState } from 'react'
import { searchIndex } from './content'
import { escapeHtml, route, type Entry } from './markdown'
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
export function Search({ onClose }: { onClose: () => void }) {
  const [needle, setNeedle] = useState('')
  const [selected, setSelected] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  const index = useMemo(() => searchIndex(), [])

  useEffect(() => {
    input.current?.focus()
  }, [])

  const words = useMemo(
    () => needle.trim().toLowerCase().split(/\s+/).filter(Boolean),
    [needle],
  )

  const hits = useMemo(() => {
    if (words.length === 0) return []
    return index
      .map((entry) => ({ entry, score: score(entry, words) }))
      .filter((hit) => hit.score > 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, 24)
  }, [index, words])

  const open = (entry: Entry) => {
    navigate(route(entry.slug) + (entry.anchor ? `#${entry.anchor}` : ''))
    onClose()
  }

  return (
    <div
      className="search-modal"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose()
      }}
    >
      <div className="search-panel" role="dialog" aria-modal="true" aria-label="Search the documentation">
        <input
          ref={input}
          type="search"
          placeholder="Search the documentation…"
          autoComplete="off"
          spellCheck={false}
          value={needle}
          onChange={(event) => {
            setNeedle(event.target.value)
            setSelected(0)
          }}
          onKeyDown={(event) => {
            if (event.key === 'Escape') return onClose()
            if (event.key === 'ArrowDown') {
              event.preventDefault()
              setSelected((at) => Math.min(at + 1, hits.length - 1))
            }
            if (event.key === 'ArrowUp') {
              event.preventDefault()
              setSelected((at) => Math.max(at - 1, 0))
            }
            if (event.key === 'Enter' && hits[selected]) {
              event.preventDefault()
              open(hits[selected].entry)
            }
          }}
        />
        <ul className="results">
          {words.length === 0 && <li className="empty">Type to search titles and body text.</li>}
          {words.length > 0 && hits.length === 0 && <li className="empty">Nothing matched.</li>}
          {hits.map(({ entry }, position) => (
            <li key={`${entry.slug}#${entry.anchor}`} className={position === selected ? 'sel' : undefined}>
              <a
                href={route(entry.slug) + (entry.anchor ? `#${entry.anchor}` : '')}
                onMouseEnter={() => setSelected(position)}
                onClick={(event) => {
                  event.preventDefault()
                  open(entry)
                }}
                dangerouslySetInnerHTML={{
                  __html:
                    mark(entry.title, words[0]) +
                    `<small>${escapeHtml(entry.page)} · ${mark(snippet(entry.text, words[0]), words[0])}</small>`,
                }}
              />
            </li>
          ))}
        </ul>
        <p className="search-hint">Enter opens · Esc closes · ↑↓ moves</p>
      </div>
    </div>
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

function mark(text: string, needle: string): string {
  const at = text.toLowerCase().indexOf(needle)
  if (at < 0 || !needle) return escapeHtml(text)
  return (
    escapeHtml(text.slice(0, at)) +
    `<em>${escapeHtml(text.slice(at, at + needle.length))}</em>` +
    escapeHtml(text.slice(at + needle.length))
  )
}

function snippet(text: string, needle: string): string {
  if (!needle) return text.slice(0, 120)
  const at = text.toLowerCase().indexOf(needle)
  if (at < 0) return text.slice(0, 120)
  const from = Math.max(0, at - 45)
  return (from ? '…' : '') + text.slice(from, from + 150)
}
