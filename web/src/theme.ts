import { defineTheme } from '@astryxdesign/core/theme'
import { defineSyntaxTheme } from '@astryxdesign/core/theme/syntax'

/**
 * **Fjord, as a theme** — a pastel red, at every lightness it has to live at.
 *
 * The design system is Astryx; the palette is this file, and it is one hue
 * family carried all the way through: the accent is a dusty rose-red, the
 * neutrals carry a whisper of the same hue (15°) so the page belongs to the
 * accent rather than sitting under it, and the code colours are chosen against
 * both.
 *
 * **Designed in OKLCH, written as hex.** A ramp of hand-picked hexes drifts;
 * what is chosen here is the *distance* between the steps rather than the
 * values. Light: muted 96.4, page 98.4, card 100, so a card lifts off the page
 * and a toolbar sits into it; inks at 23, 47, 67. Dark: muted 16.5, page 19,
 * card 23; inks at 92, 74, 56 — the same ladder, upside down. Every syntax
 * colour sits at one lightness per scheme (46 light, 80–82 dark), so a keyword
 * and a string differ in *hue* rather than in weight.
 *
 * Each value carries the OKLCH it came from, because that is the number worth
 * editing. They are hex rather than `oklch()` for one reason: the accent is a
 * *seed* the theme reads to derive `--color-on-accent` and the accent inks, and
 * a form it cannot parse gives a magenta eyebrow and no warning.
 *
 * The one collision worth knowing about: error and accent are both red. They are
 * separated by saturation and by lightness — the accent carries chroma .165, an
 * error is darker and the most saturated red on the page at .22 — because in a
 * red theme a *quieter* error would be the thing nobody sees.
 */
const syntax = defineSyntaxTheme({
  name: 'fjord-code',
  tokens: {
    // The keys are Astryx's; the classes are `fjord_inspect::tokens`' decisions.
    keyword: ['#663e9e', '#c7aff5'], // violet: 46% .15 300 / 80% .10 300
    string: ['#1d6835', '#95d7a2'], // green: 46% .11 150 / 82% .10 150
    number: ['#006768', '#83d5d4'], // teal: 46% .09 195 / 82% .08 195
    comment: ['#8d8384', '#807878'], // 62% .012 15 / 58% .010 15
    function: ['#005a9d', '#91c3f6'], // blue: 46% .13 250 / 80% .09 250
    type: ['#005a9d', '#91c3f6'],
    // `variable` is the design system's *plain* code colour — what an
    // unhighlighted block is painted with — so it is the ink, and a sigla
    // variable takes `constant` below.
    variable: ['#221b1b', '#eae2e3'],
    constant: ['#a82037', '#fda0a3'], // a sigla variable: 48% .17 18 / 80% .11 18
    property: ['#332c2c', '#ddd5d6'], // 30% .010 15 / 88% .008 15
    operator: ['#786f6f', '#989090'], // 55% .012 15 / 66% .010 15
    punctuation: ['#786f6f', '#989090'],
    attribute: ['#332c2c', '#ddd5d6'],
    // A byte the lexer refused. The most saturated red on the page, because it
    // has to be found in a page whose accent is also red.
    tag: ['#b30000', '#f97165'], // 46% .22 30 / 71% .17 27
    background: ['#faf5f5', '#110c0c'], // 97.4% .006 15 / 16% .008 15
  },
})

export const fjordTheme = defineTheme({
  name: 'fjord',
  color: {
    accent: ['#c2404e', '#f49194'], // 56% .165 18 / 76% .12 18
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
    '--color-background-body': ['#fdf8f9', '#171213'], // 98.4% .005 15 / 19% .008 15
    '--color-background-surface': ['#ffffff', '#211b1b'], // 100 / 23% .010 15
    '--color-background-card': ['#ffffff', '#211b1b'],
    '--color-background-muted': ['#f8f1f1', '#120d0d'], // 96.4% .008 15 / 16.5% .008 15
    '--color-background-popover': ['#ffffff', '#292222'], // 26% .010 15

    '--color-text-primary': ['#221b1b', '#eae2e3'], // 23% .012 15 / 92% .008 15
    '--color-text-secondary': ['#625858', '#b1a8a9'], // 47% .014 15 / 74% .010 15
    '--color-text-disabled': ['#9b9393', '#7a7272'], // 67% .010 15 / 56% .010 15

    // A hairline that is visible on white without drawing a box around itself,
    // and a stronger one for the edges that separate regions.
    '--color-border': ['#e3dcdc', '#332c2c'], // 90% .008 15 / 30% .010 15
    '--color-border-emphasized': ['#c5bbbb', '#4e4546'], // 80% .012 15 / 40% .012 15

    // The wash every "this is the row the machine is on" highlight is made from.
    // Alpha rather than a solid, because it lands on white, on the muted band
    // and on a striped table, and has to be the same wash on all three.
    '--color-accent-muted': ['#e45b6729', '#f4919433'], // 65% .17 18 at 16% / 20%
  },
})
