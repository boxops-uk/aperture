import { useEffect, useState } from 'react'
import type { Heading } from './markdown'
import { navigate } from './router'

/**
 * On this page — and where on it you are.
 *
 * The mark is *the last heading above the fold*, not the first one on screen: a
 * short section scrolled past its own heading still belongs to that heading,
 * and a reader who sees the highlight jump ahead of them stops trusting it.
 */
export function Toc({ toc, slug }: { toc: Heading[]; slug: string }) {
  const [active, setActive] = useState<string | null>(null)

  useEffect(() => {
    if (toc.length < 2) return
    let queued = false

    const mark = () => {
      let current: string | null = null
      for (const { anchor } of toc) {
        const element = document.getElementById(anchor)
        if (element && element.getBoundingClientRect().top < 120) current = anchor
      }
      setActive(current ?? toc[0].anchor)
    }

    const onScroll = () => {
      if (queued) return
      queued = true
      requestAnimationFrame(() => {
        queued = false
        mark()
      })
    }

    mark()
    window.addEventListener('scroll', onScroll, { passive: true })
    return () => window.removeEventListener('scroll', onScroll)
  }, [toc, slug])

  if (toc.length < 2) return <aside className="toc" aria-label="On this page" />

  return (
    <aside className="toc" aria-label="On this page">
      <p className="toc-label">On this page</p>
      <ul>
        {toc.map(({ level, anchor, text }) => (
          <li key={anchor} className={`lvl${level}`}>
            <a
              href={`#${anchor}`}
              className={active === anchor ? 'active' : undefined}
              onClick={(event) => {
                event.preventDefault()
                navigate(`#${anchor}`)
              }}
            >
              {text}
            </a>
          </li>
        ))}
      </ul>
    </aside>
  )
}
