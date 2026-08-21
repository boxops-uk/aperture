import type { ReactNode } from 'react'
import { Dialog, DialogHeader } from '@astryxdesign/core/Dialog'
import { VStack } from '@astryxdesign/core/Stack'

/**
 * A dialog over the page, for what is context rather than work.
 *
 * The schema lives here: a reader edits it rarely and reads it often, and giving
 * it a column of its own costs the width the database table needs. `purpose`
 * decides the dismissal rules — this one is `info`, so Escape and a click
 * outside both close it, because a panel that traps you is worse than no panel.
 */
export function Drawer({
  open,
  summary,
  onClose,
  children,
}: {
  open: boolean
  /** What the schema says about itself, in the header rather than beside it. */
  summary: ReactNode
  onClose: () => void
  children: ReactNode
}) {
  return (
    <Dialog
      isOpen={open}
      onOpenChange={(next) => {
        if (!next) onClose()
      }}
      purpose="info"
      width={760}
      maxHeight="84vh"
    >
      <DialogHeader
        title="Schema"
        subtitle={typeof summary === 'string' ? summary : undefined}
        hasDivider
        onOpenChange={(next) => {
          if (!next) onClose()
        }}
      />
      <VStack isScrollable padding={4} gap={3}>
        {children}
      </VStack>
    </Dialog>
  )
}
