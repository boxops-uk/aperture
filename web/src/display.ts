/** Whitespace has to be visible in a table, or a row reads as empty. */
export function display(text: string): string {
  return text.replace(/\n/g, '⏎').replace(/\t/g, '⇥').replace(/ /g, '·')
}
