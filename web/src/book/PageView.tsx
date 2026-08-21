import { useEffect } from 'react'
import { Code } from './Code'
import { Demo } from '../demo/Demo'
import { inline, route } from './markdown'
import { navTitle, neighbours, page as findPage, rendered } from './content'
import { navigate, scrollTo } from './router'

const SITE = 'Fjord DB'

/**
 * One page of the book: the prose as it was written, with the demos running.
 *
 * The prose arrives as HTML, because that is what markdown is; the code blocks
 * and the demos arrive as components, because they are the parts that do
 * something. Rendering them in one list keeps them in the order the author
 * wrote — a demo is a paragraph of the argument, not a sidebar.
 */
export function PageView({ slug, hash }: { slug: string; hash: string }) {
  const page = findPage(slug)
  const content = rendered(slug)

  useEffect(() => {
    document.title = page ? (page.slug === 'index' ? SITE : `${page.title} · ${SITE}`) : SITE
  }, [page])

  // A fragment names a heading that only exists once this page has rendered.
  useEffect(() => {
    if (hash) scrollTo(hash)
    else window.scrollTo({ top: 0 })
  }, [slug, hash])

  if (!page || !content) {
    return (
      <article className="prose">
        <h1>Not a page</h1>
        <p className="lede">
          There is no <code>{slug}</code> in the book. <a href={route('index')}>Start at the
          beginning</a>.
        </p>
      </article>
    )
  }

  const { previous, next } = neighbours(slug)

  return (
    <article className="prose">
      {page.group && <p className="eyebrow">{page.group}</p>}
      <h1>{page.title}</h1>
      {page.description && (
        <p className="lede" dangerouslySetInnerHTML={{ __html: inline(page.description) }} />
      )}

      {content.blocks.map((block, index) =>
        block.kind === 'html' ? (
          <div key={index} className="flow" dangerouslySetInnerHTML={{ __html: block.html }} />
        ) : block.kind === 'code' ? (
          <Code key={index} lang={block.lang} source={block.source} />
        ) : (
          <Demo key={index} demo={block.demo} />
        ),
      )}

      <nav className="pager">
        {previous ? (
          <a
            className="prev"
            href={route(previous)}
            onClick={(event) => {
              event.preventDefault()
              navigate(route(previous))
            }}
          >
            <span>Previous</span>
            {navTitle(previous)}
          </a>
        ) : (
          <span />
        )}
        {next && (
          <a
            className="next"
            href={route(next)}
            onClick={(event) => {
              event.preventDefault()
              navigate(route(next))
            }}
          >
            <span>Next</span>
            {navTitle(next)}
          </a>
        )}
      </nav>
    </article>
  )
}
