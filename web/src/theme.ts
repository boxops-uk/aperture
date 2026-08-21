import { defineTheme } from '@astryxdesign/core/theme'
import { defineSyntaxTheme } from '@astryxdesign/core/theme/syntax'

/**
 * **Fjord, as a theme.**
 *
 * The design system is Astryx; the *palette* is the book's, because the book had
 * one before it had components — warm paper, rust accent, and the code colours
 * the engine's own lexer paints with. Nothing here restyles a component: this is
 * the seed the whole token set is generated from, which is what makes a Button
 * on this site look like a Button and still look like Fjord.
 *
 * The trap it avoids is the one the theming guide names: overriding
 * `--color-*` in `:root` re-points the tokens a component reads but not the ones
 * derived from the accent seed, so the accent goes in as `color.accent` and the
 * surfaces go in as explicit token overrides.
 */
const syntax = defineSyntaxTheme({
  name: 'fjord-code',
  tokens: {
    // The keys are Astryx's; the colours are the ones `fjord_inspect::tokens`
    // has been deciding all along, so a block painted by the real lexer and a
    // block painted by the fallback rules are the same colours.
    keyword: ['#8a3a8a', '#d08fd0'],
    string: ['#2f6b3a', '#8fc99a'],
    number: ['#9a4a12', '#e0a06a'],
    comment: ['#8d8a82', '#6f6c66'],
    function: ['#1f5d8c', '#7fb6e0'],
    type: ['#1f5d8c', '#7fb6e0'],
    variable: ['#7e3f13', '#f0b681'],
    property: ['#2c2a26', '#dcd9d3'],
    constant: ['#8d8a82', '#6f6c66'],
    operator: ['#6a6760', '#8b8880'],
    punctuation: ['#6a6760', '#8b8880'],
    attribute: ['#2c2a26', '#dcd9d3'],
    // A byte the lexer refused. It has a colour because it has to be found.
    tag: ['#b3261e', '#f2857c'],
    background: ['#f5f3ef', '#14161a'],
  },
})

export const fjordTheme = defineTheme({
  name: 'fjord',
  color: {
    accent: ['#a2521a', '#e2934f'],
    neutralStyle: 'warm',
  },
  typography: {
    body: {
      family: '-apple-system',
      fallbacks: 'BlinkMacSystemFont, "Segoe UI", Inter, Roboto, "Helvetica Neue", Arial, sans-serif',
    },
    heading: {
      family: '-apple-system',
      fallbacks: 'BlinkMacSystemFont, "Segoe UI", Inter, Roboto, "Helvetica Neue", Arial, sans-serif',
    },
    // No webfont, here or anywhere: the site makes no external request, which is
    // a property of the published book rather than a preference.
    code: {
      family: 'ui-monospace',
      fallbacks: 'SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    },
    scale: { base: 16, ratio: 1.2 },
  },
  radius: { base: 4, multiplier: 1 },
  syntax,
  tokens: {
    '--color-background-body': ['#fbfbfa', '#16171a'],
    '--color-background-surface': ['#ffffff', '#1c1e22'],
    '--color-background-card': ['#ffffff', '#1c1e22'],
    '--color-background-muted': ['#f2f1ee', '#121316'],
    '--color-text-primary': ['#1b1b1a', '#e8e6e1'],
    '--color-text-secondary': ['#55534e', '#b1aea7'],
    '--color-border': ['#e3e1dc', '#2a2d33'],
    '--color-border-emphasized': ['#cfccc4', '#3b3f47'],
  },
})
