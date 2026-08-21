import { Layout } from './book/Layout'
import { PageView } from './book/PageView'
import { rendered } from './book/content'
import { slugOf, useLocation } from './book/router'
import { Playground } from './Playground'
import './book.css'

/**
 * **The site**: the design book, and the engine that the book is about.
 *
 * One application rather than a site plus a demo. The pages are the same
 * Markdown the generated site publishes, so there is one book; the demos in
 * them are the same engine the playground runs, so there is one engine. What a
 * paragraph claims, the panel under it does.
 */
export default function App() {
  const location = useLocation()
  const [pathname, hash = ''] = splitHash(location)
  const slug = slugOf(pathname)

  if (slug === 'playground') {
    return (
      <Layout slug={slug} toc={[]} fills>
        <Playground />
      </Layout>
    )
  }

  return (
    <Layout slug={slug} toc={rendered(slug)?.toc ?? []}>
      <PageView slug={slug} hash={hash} />
    </Layout>
  )
}

function splitHash(location: string): [string, string] {
  const at = location.indexOf('#')
  return at === -1 ? [location, ''] : [location.slice(0, at), location.slice(at)]
}
