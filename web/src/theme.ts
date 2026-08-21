import { defineTheme } from '@astryxdesign/core/theme'
import { defineSyntaxTheme } from '@astryxdesign/core/theme/syntax'

/**
 * **Fjord, as a theme.**
 *
 * The design system is Astryx; the *palette* is the book's, because the book had
 * one before it had components — warm paper, a rust accent, and the code colours
 * the engine's own lexer paints with. Nothing here restyles a component: this is
 * the seed the whole token set is generated from, which is what makes a Button
 * on this site look like a Button and still look like Fjord.
 *
 * **Designed in OKLCH, written as hex.** A ramp of hand-picked hexes drifts —
 * the light scheme read as dirty paper because its greys went beige at one end
 * and grey at the other, and nothing held the ladder together. So the light
 * scheme is one hue (70°, warm enough to be paper and far short of beige) at one
 * small chroma, and what is *chosen* is the distance between the steps rather
 * than the values: muted 96.2, page 98.2, card 100, so a card lifts off the page
 * and a toolbar sits into it; inks at 22, 46 and 66. Each comment carries the
 * OKLCH the hex came from, because that is the number worth editing.
 *
 * They are hex rather than `oklch()` for one reason: the accent is a *seed*, and
 * the theme derives `--color-on-accent` and the accent inks from it by reading
 * the colour. A form it cannot read produces a magenta eyebrow and no warning.
 *
 * The dark scheme was already right and is left as it was, value for value.
 */
const syntax = defineSyntaxTheme({
  name: 'fjord-code',
  tokens: {
    // The keys are Astryx's; the colours are `fjord_inspect::tokens`' decisions.
    // Every light value sits at 44–48% lightness so no token shouts over the
    // others — a keyword and a string differ in *hue*, not in weight.
    keyword: ['#693996', '#d08fd0'], // 45% .15 305
    string: ['#1d6835', '#8fc99a'], // 46% .11 150
    number: ['#904700', '#e0a06a'], // 48% .13 60
    comment: ['#8b857f', '#6f6c66'], // 62% .012 70
    function: ['#005a9d', '#7fb6e0'], // 46% .13 250
    type: ['#005a9d', '#7fb6e0'],
    // `variable` is the design system's *plain* code colour — what an
    // unhighlighted block is painted with — so it is the ink, and a sigla
    // variable takes `constant` below. A rust `variable` turned every plaintext
    // block on the site rust.
    variable: ['#1e1a15', '#e8e6e1'],
    property: ['#312d28', '#dcd9d3'], // 30% .010 70
    constant: ['#863709', '#f0b681'], // a sigla variable: 44% .12 45
    operator: ['#76706a', '#8b8880'], // 55% .012 70
    punctuation: ['#76706a', '#8b8880'],
    attribute: ['#312d28', '#dcd9d3'],
    // A byte the lexer refused. It has a colour because it has to be found.
    tag: ['#b71824', '#f2857c'], // 50% .19 25
    background: ['#f7f5f3', '#14161a'], // 97.2% .004 70
  },
})

export const fjordTheme = defineTheme({
  name: 'fjord',
  color: {
    accent: ['#a54b0e', '#e2934f'], // 52% .135 48
    neutralStyle: 'warm',
  },
  typography: {
    // No webfont, here or anywhere: the site makes no external request, which is
    // a property of the published book rather than a preference. The stack ends
    // in the platform UI faces so the fallback is a choice rather than whatever
    // the browser reaches for.
    body: {
      family: 'ui-sans-serif',
      fallbacks: 'system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif',
    },
    heading: {
      family: 'ui-sans-serif',
      fallbacks: 'system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif',
    },
    code: {
      family: 'ui-monospace',
      fallbacks: 'SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    },
    scale: { base: 16, ratio: 1.2 },
  },
  radius: { base: 4, multiplier: 1 },
  syntax,
  tokens: {
    // The ladder: a toolbar sits into the page, a card lifts off it.
    '--color-background-body': ['#faf9f7', '#16171a'], // 98.2% .003 70
    '--color-background-surface': ['#ffffff', '#1c1e22'],
    '--color-background-card': ['#ffffff', '#1c1e22'],
    '--color-background-muted': ['#f4f2f0', '#121316'], // 96.2% .004 70
    '--color-background-popover': ['#ffffff', '#22252a'],

    '--color-text-primary': ['#1e1a15', '#e8e6e1'], // 22% .012 70
    '--color-text-secondary': ['#5d5750', '#b1aea7'], // 46% .014 70
    '--color-text-disabled': ['#96918c', '#85817a'], // 66% .010 70

    // A hairline that is visible on white without drawing a box around itself,
    // and a stronger one for the edges that separate regions.
    '--color-border': ['#e1ddda', '#2a2d33'], // 90% .006 70
    '--color-border-emphasized': ['#c1bdb8', '#3b3f47'], // 80% .008 70

    // The wash every "this is the row the machine is on" highlight is made from.
    // Alpha rather than a solid, because it lands on white, on the muted band
    // and on a striped table, and has to be the same wash on all three.
    '--color-accent-muted': ['#bb5c2229', '#e2934f3f'], // 58% .14 48 at 16%
  },
})
