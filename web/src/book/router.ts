/**
 * **Routing, in one file.**
 *
 * A page is a path, not a fragment: `#i5` has to stay the anchor a heading
 * links to, and the book is full of them. The pages are known at build time and
 * there are twenty of them, so this is a URL, a listener and a lookup — a
 * router library would be more code than the site it routes.
 *
 * The trap is the served copy: a path route needs the host to answer every path
 * with the same document. `dist/404.html` is that answer for GitHub Pages, and
 * `vite preview` and the dev server do it by themselves.
 */
import { useSyncExternalStore } from 'react'

const listeners = new Set<() => void>()

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  window.addEventListener('popstate', listener)
  return () => {
    listeners.delete(listener)
    window.removeEventListener('popstate', listener)
  }
}

function snapshot(): string {
  return window.location.pathname + window.location.hash
}

/** The current path and fragment, re-read whenever either changes. */
export function useLocation(): string {
  return useSyncExternalStore(subscribe, snapshot, () => '/')
}

export function navigate(to: string, replace = false): void {
  const url = new URL(to, window.location.href)
  if (url.origin !== window.location.origin) {
    window.location.href = to
    return
  }
  const same = url.pathname === window.location.pathname
  if (same && url.hash === window.location.hash) {
    scrollTo(url.hash)
    return
  }
  window.history[replace ? 'replaceState' : 'pushState'](null, '', url)
  for (const listener of listeners) listener()
  if (!same) window.scrollTo({ top: 0 })
  scrollTo(url.hash)
}

/** Bring a fragment into view, after the page it names has rendered. */
export function scrollTo(hash: string): void {
  if (!hash) return
  requestAnimationFrame(() => {
    document.getElementById(hash.slice(1))?.scrollIntoView({ block: 'start' })
  })
}

/** Which page a path names. The site's root is the book's first page. */
export function slugOf(pathname: string): string {
  const base = import.meta.env.BASE_URL
  const path = (pathname.startsWith(base) ? pathname.slice(base.length) : pathname.replace(/^\//, ''))
    .replace(/\/$/, '')
    // A path the generated site would serve, followed in from somewhere older.
    .replace(/\.html$/, '')
  return path === '' ? 'index' : path
}
