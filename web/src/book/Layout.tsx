import { useEffect, useState, type ReactNode } from 'react'
import { GROUPS } from './content'
import type { Heading } from './markdown'
import { route } from './markdown'
import { navigate } from './router'
import { useTheme } from './theme'
import { Search } from './Search'
import { Toc } from './Toc'

/**
 * The shell: a bar, the reading order, the page, and where you are in it.
 *
 * Two shapes, one shell. A page of the book scrolls between a sidebar and a
 * table of contents; the playground is an application that owns the viewport
 * and cannot share it with either. The bar is the same in both, because it is
 * how a reader gets from one to the other.
 */
export function Layout({
  slug,
  toc,
  fills,
  children,
}: {
  slug: string
  toc: Heading[]
  /** The page is an application: it takes the height and does its own scrolling. */
  fills?: boolean
  children: ReactNode
}) {
  const [searching, setSearching] = useState(false)
  const [menu, setMenu] = useState(false)
  const { toggle } = useTheme()

  // `/` and ⌘K open search from anywhere that is not already taking the key.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setSearching(false)
        setMenu(false)
        return
      }
      const target = event.target as HTMLElement | null
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable
      if (typing) return
      if (event.key === '/' || ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k')) {
        event.preventDefault()
        setSearching(true)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  // The prose is HTML, so its links are not components: one listener at the top
  // keeps every link in the book a navigation rather than a page load.
  useEffect(() => {
    const onClick = (event: MouseEvent) => {
      if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey) return
      const link = (event.target as HTMLElement | null)?.closest('a')
      if (!link) return
      const href = link.getAttribute('href')
      if (!href || link.target === '_blank') return
      if (/^(https?:|mailto:)/.test(href)) return
      event.preventDefault()
      // Following a link closes the menu it may have been followed from: on a
      // phone the sidebar is over the page it is navigating to.
      setMenu(false)
      navigate(href)
    }
    document.addEventListener('click', onClick)
    return () => document.removeEventListener('click', onClick)
  }, [])

  return (
    <>
      <a className="skip" href="#main">
        Skip to content
      </a>
      <header className="topbar">
        <button
          className={fills ? 'menu always' : 'menu'}
          type="button"
          aria-label="Menu"
          aria-expanded={menu}
          onClick={() => setMenu((open) => !open)}
        >
          ☰
        </button>
        <a className="brand" href={route('index')}>
          <span className="mark" aria-hidden="true" />
          <span className="brand-text">
            <b>Fjord DB</b>
            <i>An embedded, immutable fact database</i>
          </span>
        </a>
        <button className="search-open" type="button" onClick={() => setSearching(true)}>
          <span>Search</span>
          <kbd>/</kbd>
        </button>
        <button className="theme" type="button" aria-label="Toggle colour scheme" onClick={toggle}>
          ◐
        </button>
      </header>

      <div className={fills ? 'layout fills' : 'layout'}>
        <div className={menu ? 'sidebar open' : 'sidebar'}>
          <nav className="nav" aria-label="Documentation">
            {GROUPS.map((group) => (
              <div key={group.label}>
                <p className="nav-group">{group.label}</p>
                <ul>
                  {group.pages.map((page) => (
                    <li key={page.slug}>
                      <a
                        href={route(page.slug)}
                        className={page.slug === slug ? 'current' : undefined}
                        aria-current={page.slug === slug ? 'page' : undefined}
                      >
                        {page.title}
                      </a>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </nav>
        </div>

        <main id="main">{children}</main>

        {!fills && <Toc toc={toc} slug={slug} />}
      </div>

      {searching && <Search onClose={() => setSearching(false)} />}
    </>
  )
}
