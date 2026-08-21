import type { ReactNode } from 'react'
import { Collapsible } from '@astryxdesign/core/Collapsible'
import { Text } from '@astryxdesign/core/Text'
import { HStack } from '@astryxdesign/core/Stack'

/**
 * One section of the left-hand stack.
 *
 * An accordion rather than tabs, because the views are meant to be read
 * *against each other*: the plan beside the run it is executing, the lowered
 * tree beside the plan it produced. Tabs make that a memory exercise.
 */
export function Section({
  name,
  count,
  open,
  onToggle,
  children,
}: {
  name: string
  count?: ReactNode
  open: boolean
  onToggle: () => void
  children: ReactNode
}) {
  return (
    <Collapsible
      isOpen={open}
      onOpenChange={onToggle}
      trigger={
        <HStack gap={2} align="center" justify="between" width="100%">
          <Text type="label" weight="semibold">
            {name}
          </Text>
          {count !== undefined && (
            <Text type="supporting" hasTabularNumbers>
              {count}
            </Text>
          )}
        </HStack>
      }
    >
      {children}
    </Collapsible>
  )
}
