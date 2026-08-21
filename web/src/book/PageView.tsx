import { useEffect, type ReactNode } from 'react'
import { Heading } from '@astryxdesign/core/Heading'
import { Text } from '@astryxdesign/core/Text'
import { Link } from '@astryxdesign/core/Link'
import { Code as InlineCode } from '@astryxdesign/core/Code'
import { List, ListItem } from '@astryxdesign/core/List'
import { Table, TableCell, TableHeaderCell, TableRow } from '@astryxdesign/core/Table'
import { Banner } from '@astryxdesign/core/Banner'
import { Blockquote } from '@astryxdesign/core/Blockquote'
import { Divider } from '@astryxdesign/core/Divider'
import { Card } from '@astryxdesign/core/Card'
import { Grid } from '@astryxdesign/core/Grid'
import { Section } from '@astryxdesign/core/Section'
import { VStack } from '@astryxdesign/core/Stack'
import { Code } from './Code'
import { Demo } from '../demo/Demo'
import { inlines, type Block, type Inline } from './markdown'
import { navTitle, neighbours, page as findPage, rendered } from './content'
import { route } from './markdown'
import { scrollTo } from './router'

const SITE = 'Fjord DB'

/**
 * One page of the book: the prose as it was written, with the demos running.
 *
 * Every block is a component — a table is a `Table`, a callout is a `Banner`, a
 * fence is a `CodeBlock` — so the page inherits the design system's type scale,
 * spacing and dark mode rather than a stylesheet's opinion of them. The one
 * exception is a raw HTML block, which the dialect allows and the home page
 * uses; it arrives as the string it was written as.
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
      <Section padding={6} maxWidth={780}>
        <VStack gap={3}>
          <Heading level={1}>Not a page</Heading>
          <Text type="large" color="secondary">
            There is no <InlineCode>{slug}</InlineCode> in the book.{' '}
            <Link href={route('index')}>Start at the beginning</Link>.
          </Text>
        </VStack>
      </Section>
    )
  }

  const { previous, next } = neighbours(slug)

  return (
    <Section padding={6} maxWidth={780} data-testid="prose">
      <VStack gap={4} align="stretch">
        {page.group && (
          <Text type="label" color="accent" weight="bold">
            {page.group.toUpperCase()}
          </Text>
        )}
        <Heading level={1} type="display-2">
          {page.title}
        </Heading>
        {page.description && (
          <Text size="lg" color="secondary">
            {marks(inlines(page.description))}
          </Text>
        )}

        {content.blocks.map((block, index) => (
          <BlockView key={index} block={block} />
        ))}

        <Divider />

        <Grid columns={2} gap={3}>
          {previous ? (
            <Card padding={3}>
              <Link href={route(previous)} data-testid="pager-prev">
                <VStack gap={0.5}>
                  <Text type="supporting">Previous</Text>
                  <Text weight="semibold">{navTitle(previous)}</Text>
                </VStack>
              </Link>
            </Card>
          ) : (
            <span />
          )}
          {next && (
            <Card padding={3}>
              <Link href={route(next)} data-testid="pager-next">
                <VStack gap={0.5} align="end">
                  <Text type="supporting">Next</Text>
                  <Text weight="semibold">{navTitle(next)}</Text>
                </VStack>
              </Link>
            </Card>
          )}
        </Grid>
      </VStack>
    </Section>
  )
}

/** The tone a callout carries, as the design system's four statuses. */
const STATUS: Record<string, 'info' | 'warning' | 'error' | 'success'> = {
  note: 'info',
  warn: 'warning',
  invariant: 'success',
  gap: 'warning',
}

function BlockView({ block }: { block: Block }): ReactNode {
  switch (block.kind) {
    case 'heading':
      return (
        <Heading level={block.level as 2 | 3 | 4} id={block.anchor}>
          {marks(block.children)}
        </Heading>
      )

    case 'para':
      return <Text as="p">{marks(block.children)}</Text>

    case 'list':
      return (
        <List listStyle={block.ordered ? 'decimal' : 'disc'} density="compact">
          {block.items.map((item, index) => (
            <ListItem
              key={index}
              label={
                <VStack gap={1}>
                  <Text>{marks(item.children)}</Text>
                  {item.nested.length > 0 && (
                    <List listStyle="circle" density="compact">
                      {item.nested.map((nested, at) => (
                        <ListItem key={at} label={<Text>{marks(nested)}</Text>} />
                      ))}
                    </List>
                  )}
                </VStack>
              }
            />
          ))}
        </List>
      )

    case 'table':
      return (
        <Table density="compact" verticalAlign="top">
          <TableRow isHeaderRow>
            {block.head.map((cell, index) => (
              <TableHeaderCell key={index}>{marks(cell)}</TableHeaderCell>
            ))}
          </TableRow>
          {block.rows.map((row, index) => (
            <TableRow key={index}>
              {row.map((cell, at) => (
                <TableCell key={at}>{marks(cell)}</TableCell>
              ))}
            </TableRow>
          ))}
        </Table>
      )

    case 'quote':
      return (
        <Blockquote>
          <VStack gap={2}>
            {block.blocks.map((inner, index) => (
              <BlockView key={index} block={inner} />
            ))}
          </VStack>
        </Blockquote>
      )

    case 'callout':
      return (
        <Banner status={STATUS[block.tone] ?? 'info'} title={marks(block.label)} defaultIsExpanded>
          <VStack gap={2}>
            {block.blocks.map((inner, index) => (
              <BlockView key={index} block={inner} />
            ))}
          </VStack>
        </Banner>
      )

    case 'rule':
      return <Divider />

    case 'code':
      return <Code lang={block.lang} source={block.source} />

    case 'demo':
      return <Demo demo={block.demo} />

    case 'html':
      // Authored HTML, which the dialect allows and the home page's card grid
      // is written as. It is content, not layout this file decides.
      return <Authored html={block.html} />
  }
}

/**
 * A block of HTML the author wrote, which the dialect allows and the home page's
 * card grid is written as. It is content rather than layout this file decides,
 * so it is rendered as written and styled by the two class names it uses.
 */
function Authored({ html }: { html: string }) {
  return <div className="authored" dangerouslySetInnerHTML={{ __html: html }} />
}

/** Inline marks, as components rather than as tags. */
function marks(nodes: Inline[]): ReactNode {
  return nodes.map((node, index) => {
    switch (node.kind) {
      case 'text':
        return node.text
      case 'code':
        return <InlineCode key={index}>{node.text}</InlineCode>
      case 'strong':
        return (
          <Text key={index} weight="semibold">
            {marks(node.children)}
          </Text>
        )
      case 'em':
        return (
          <em key={index}>{marks(node.children)}</em>
        )
      case 'del':
        return (
          <Text key={index} hasStrikethrough>
            {marks(node.children)}
          </Text>
        )
      case 'link':
        return (
          <Link key={index} href={node.href} isExternalLink={/^https?:/.test(node.href)}>
            {marks(node.children)}
          </Link>
        )
    }
  })
}
