import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { AppShell } from '@astryxdesign/core/AppShell'
import { TopNav, TopNavHeading } from '@astryxdesign/core/TopNav'
import { SideNav, SideNavItem, SideNavSection } from '@astryxdesign/core/SideNav'
import { Layout as Panes, LayoutContent, LayoutPanel } from '@astryxdesign/core/Layout'
import { Center } from '@astryxdesign/core/Center'
import { useMediaQuery } from '@astryxdesign/core/hooks'
import { Outline } from '@astryxdesign/core/Outline'
import { Button } from '@astryxdesign/core/Button'
import { IconButton } from '@astryxdesign/core/IconButton'
import { Kbd } from '@astryxdesign/core/Kbd'
import { HStack } from '@astryxdesign/core/Stack'
import { GROUPS } from './content'
import type { Heading } from './markdown'
import { route } from './markdown'
import { navigate } from './router'
import { Search } from './Search'
import { ContrastIcon } from './ContrastIcon'

/**
 * The shell: a bar, the reading order, the page, and where you are in it.
 *
 * Responsive contract, at the frame root:
 *   > 1200px  nav 260 | prose (centred, 880) | outline 300
 *   <= 1200px the outline drops rather than squeezing the measure
 *   <= 768px  the side nav collapses into AppShell's mobile drawer
 *
 * Two shapes, one shell. A page of the book scrolls between the nav and its own
 * contents; the workbench is an application that owns the viewport and cannot
 * share it with an outline. `height` is what separates them — `auto` lets a page
 * grow, `fill` hands the viewport to the panes inside it.
 */
export function Layout({
  slug,
  toc,
  fills,
  onToggleMode,
  children,
}: {
  slug: string
  toc: Heading[]
  /** The page is an application: it takes the height and does its own scrolling. */
  fills?: boolean
  onToggleMode: () => void
  children: ReactNode
}) {
  const [searching, setSearching] = useState(false)
  // The outline is the first thing to go: below this the three regions cannot
  // all have their width, and the one a reader can do without is the one that
  // only says where they are.
  const roomForTheOutline = useMediaQuery('(min-width: 1200px)')

  // `/` and ⌘K open search from anywhere that is not already taking the key.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
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

  // Most links in the prose are rendered by the markdown renderer rather than
  // written here: one listener at the top keeps every one of them a navigation
  // rather than a page load.
  useEffect(() => {
    const onClick = (event: MouseEvent) => {
      if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey) return
      const link = (event.target as HTMLElement | null)?.closest('a')
      if (!link) return
      const href = link.getAttribute('href')
      if (!href || link.target === '_blank') return
      if (/^(https?:|mailto:)/.test(href)) return
      event.preventDefault()
      navigate(href)
    }
    document.addEventListener('click', onClick)
    return () => document.removeEventListener('click', onClick)
  }, [])

  // The page scrolls inside the shell, so the outline has to be told which box
  // moves: left to find one itself it tracks the window, which never scrolls
  // here, and every heading reads as "above the line".
  const column = useRef<HTMLDivElement>(null)

  const items = useMemo(
    () => toc.map(({ anchor, text, level }) => ({ id: anchor, label: text, level })),
    [toc],
  )

  return (
    <>
      {/* `fill` for both shapes: the shell owns the viewport and the content
          column scrolls inside it, which is what keeps the reading order and
          the outline in place while a page moves under them. */}
      <AppShell
        height="fill"
        contentPadding={0}
        variant="section"
        topNav={
          <TopNav
            label="Site"
            heading={
              <TopNavHeading
                heading="Fjord DB"
                subheading="An embedded, immutable fact database"
                headingHref={route('index')}
              />
            }
            endContent={
              <HStack gap={2} align="center">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => setSearching(true)}
                  label="Search"
                  endContent={<Kbd keys="/" />}
                  data-testid="search"
                />
                <IconButton
                  variant="ghost"
                  size="sm"
                  icon={<ContrastIcon width={18} height={18} />}
                  label="Toggle colour scheme"
                  onClick={onToggleMode}
                  data-testid="mode"
                />
              </HStack>
            }
          />
        }
        // Collapsible, not resizable: the reading order is a fixed list of
        // twenty-one names, so the only width worth choosing is none at all.
        sideNav={
          <SideNav collapsible>
            {GROUPS.map((group) => (
              <SideNavSection key={group.label} title={group.label}>
                {group.pages.map((page) => (
                  <SideNavItem
                    key={page.slug}
                    label={page.title}
                    href={route(page.slug)}
                    isSelected={page.slug === slug}
                  />
                ))}
              </SideNavSection>
            ))}
          </SideNav>
        }
      >
        {fills ? (
          children
        ) : (
          <Panes
            content={
              <LayoutContent padding={0} ref={column}>
                <Center axis="horizontal">{children}</Center>
              </LayoutContent>
            }
            end={
              items.length > 1 && roomForTheOutline ? (
                <LayoutPanel width={300} label="On this page" padding={4}>
                  {/* The bar overlays the top of the scroll root, so the
                      outline has to land headings below it rather than under
                      it. */}
                  <Outline
                  items={items}
                  density="compact"
                  offset={24}
                  scrollContainerRef={column}
                />
                </LayoutPanel>
              ) : undefined
            }
          />
        )}
      </AppShell>

      <Search isOpen={searching} onOpenChange={setSearching} />
    </>
  )
}
