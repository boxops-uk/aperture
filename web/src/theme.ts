import { defineTheme } from '@astryxdesign/core/theme'
import { defineSyntaxTheme } from '@astryxdesign/core/theme/syntax'

/**
 * **Fjord, as a theme** — neutral ground, one accent, three code colours.
 *
 * The page already carries a great deal of semantic colour: a badge per plan
 * step, a wash for the row the machine is standing on, a band across the bytes a
 * scan is walking, an error, a yield. So the *ground* stays out of it. Every
 * surface, line and ink here is a neutral at one hue (250°) and a chroma small
 * enough that nobody could name it — what a reader sees as colour is only ever
 * something the engine said. The neutral hue is 250° — cool by a hair, which
 * reads cleaner on a screen than a dead grey.
 *
 * **Designed in OKLCH, written as hex,** and what is chosen is the distance
 * between the steps rather than the values. Light surfaces at 96.5 / 98.5 / 100
 * and dark at 13.5 / 16 / 20.5, so a card lifts off the page and a toolbar sits
 * into it either way up; inks at 22 / 46 / 66 and 92 / 72 / 55; lines at 90 / 80
 * and 28 / 38. Each value carries the OKLCH it came from, because that is the
 * number worth editing.
 *
 * Hex rather than `oklch()` for one reason: the accent is a *seed* the theme
 * reads to derive `--color-on-accent` and the accent inks, and a form it cannot
 * parse gives a magenta eyebrow and no warning.
 */

/**
 * **Three hues, and the rest is ink.**
 *
 * A token palette drifts into a rainbow one hue at a time — violet keywords,
 * teal numbers, amber constants — and then nothing in it means anything. These
 * are the three distinctions sigla actually has: *the language* (its keywords
 * and the variables they bind), *what is being read* (a predicate, a type), and
 * *a literal*. Fields, punctuation and plain text are ink at three weights; a
 * comment is the quietest of them.
 *
 * Both schemes hold one lightness per role — 46–52% light, 80–84% dark — so a
 * keyword and a string differ in hue rather than in weight.
 */
const syntax = defineSyntaxTheme({
  name: 'fjord-code',
  tokens: {
    keyword: ['#a83442', '#ff9d9e'], // the language: 50% .15 18 / 80% .12 20
    constant: ['#9e4c51', '#fbb6b5'], // a sigla variable: 52% .11 18 / 84% .08 20
    function: ['#0c60a3', '#8cc3fc'], // a predicate: 48% .13 250 / 80% .10 250
    type: ['#0c60a3', '#8cc3fc'],
    string: ['#1d6835', '#95d7a2'], // a literal: 46% .11 150 / 82% .10 150
    number: ['#1d6835', '#95d7a2'],
    property: ['#3e4348', '#bbbec1'], // a field: 38% .010 250 / 80% .006 250
    attribute: ['#3e4348', '#bbbec1'],
    operator: ['#777b7f', '#83878b'], // 58% .008 250 / 62% .008 250
    punctuation: ['#777b7f', '#83878b'],
    comment: ['#83878b', '#777b7f'], // 62% / 58%
    // `variable` is the design system's *plain* code colour — what an
    // unhighlighted block is painted with — so it is the ink.
    variable: ['#191b1d', '#e3e5e7'],
    // A byte the lexer refused. It used to be a redder red than the accent,
    // because a quieter error is invisible on a page whose accent is already
    // red — the accent *is* that red now, so this is the same pair as
    // `--color-error` and `--color-accent` rather than a fourth one near them.
    tag: ['#df202e', '#fc5855'], // 58% .22 25 / 68% .20 25
    background: ['#f5f7f9', '#07080a'], // 97.5% .003 250 / 13.5% .006 250
  },
})

export const fjordTheme = defineTheme({
  name: 'fjord',
  color: {
    // The seed. Astryx derives the accent inks from it and does not hand back
    // what it was given — `#df202e` comes out as `#C4001C` — so the three inks
    // are stated below rather than derived. The seed still feeds everything
    // else it generates, so it is the same colour.
    accent: ['#df202e', '#fc5855'],
    neutralStyle: 'cool',
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
    '--color-background-body': ['#f9fafb', '#0b0d10'], // 98.5% / 16%
    '--color-background-surface': ['#ffffff', '#15171a'], // 100% / 20.5%
    '--color-background-card': ['#ffffff', '#15171a'],
    '--color-background-muted': ['#f2f4f5', '#07080a'], // 96.5% / 13.5%
    '--color-background-popover': ['#ffffff', '#1d2022'], // 100% / 24%

    '--color-text-primary': ['#191b1d', '#e3e5e7'], // 22% / 92%
    '--color-text-secondary': ['#55585c', '#a2a5a8'], // 46% / 72%
    '--color-text-disabled': ['#8f9397', '#6e7276'], // 66% / 55%

    '--color-border': ['#dcdee0', '#26292d'], // 90% / 28%
    '--color-border-emphasized': ['#bbbec1', '#3e4348'], // 80% / 38%

    // **One red.** An accent ink and an error ink were two reds a few degrees
    // and eight points of lightness apart — `#ae2d3f` against `#df202e` — which
    // is not a distinction anybody reads as a distinction. It is one colour now,
    // the more saturated of the two, and `--color-error` and the syntax theme's
    // `tag` are the same pair: a note's icon, a diagnostic's icon, a refused
    // byte, the caret, the scrub bar and the band under a hovered token are all
    // the same red.
    //
    // Stated rather than derived because the seed above does not survive the
    // derivation. `--color-on-accent` comes with them: it is what sits *on* a
    // solid accent fill, and it flips, because the light accent is at 58% and
    // the dark one at 68%.
    '--color-accent': ['#df202e', '#fc5855'],
    '--color-text-accent': ['#df202e', '#fc5855'],
    '--color-icon-accent': ['#df202e', '#fc5855'],
    '--color-on-accent': ['#ffffff', '#0b0d10'],

    // The design system's own selection wash: a `:::note` banner's ground, a
    // command-palette row under the cursor, a selected item. It is the page's
    // wash, like every other ground here — it was a 16% tint of a lightened
    // accent, which composited to almost exactly this on white and to something
    // else on anything that was not white, so a note and a diagnostic were two
    // pinks that only agreed by luck about what they were sitting on.
    '--color-accent-muted': 'var(--fj-wash)',

    // **The badges draw from the same three hues as the code.** A `seek` chip
    // and a string literal being different greens is the kind of thing nobody
    // reports and everybody feels.
    //
    // Their grounds are **opaque**. A translucent tint composites with whatever
    // is behind it, and a pill lands on a row that is washed accent as often as
    // on a plain one — where the two colours mix into mud. Light tints at 92%
    // lightness, dark ones at 30%, both a step from the surface they sit on.
    '--color-text-blue': ['#00437f', '#afd5fe'],
    '--color-background-blue': ['#cfe8ff', '#162f48'],
    '--color-border-blue': ['#0c60a3', '#8cc3fc'],
    '--color-text-green': ['#004b1e', '#ace0b6'],
    '--color-background-green': ['#d0eed5', '#17351f'],
    '--color-border-green': ['#1d6835', '#95d7a2'],
    '--color-text-red': ['#7c1021', '#feb9b8'],
    '--color-background-red': ['#ffd8d7', '#472021'],
    '--color-border-red': ['#a83442', '#ff9d9e'],

    // The same again for the pill with no hue: `--color-neutral` ships as an
    // alpha grey, which muddies for the same reason.
    '--color-neutral': ['#e6e8ea', '#27292c'],

    // **A status is the palette's hue at the design system's strength.** A
    // callout, a diagnostic banner, a refused byte, a dropped row and a status
    // pill all draw from `--color-{status}`, and shipped those are a crimson, an
    // amber and a bright green that appear nowhere else here — so a warning in
    // the book and an error in the workbench belonged to different palettes.
    //
    // The hues are this page's: red at 25° (the one a refused byte is painted
    // in), green at 150° (a literal), and one that is no code colour at all. The
    // **chroma is not the code inks'**, and that is the point: a code ink is
    // read as a string of glyphs and stays muted so a page of it is calm, while
    // a status ink is one icon that has to be found at a glance. So these sit at
    // or near the gamut edge for their hue — 58% .22 / 68% .20 for the error,
    // 54% .148 / 70% .19 for the success — which is the strength the shipped
    // values carried, in the hues this page uses.
    //
    // **A status ground is the page's wash** — 94% .024 and 33% .034, which is
    // `--fj-wash` in `app.css`, where every wash here is declared. So a banner
    // sits at exactly the weight a highlighted row does, and only its hue says
    // which kind it is. The error's *is* that value, by name rather than by a
    // copied pair: its hue is the accent's, so two nearly-identical reds a few
    // degrees apart would read as one colour done twice. Opaque for the same
    // reason the rest are — a banner lands on the body, on a card and inside a
    // demo.
    '--color-error': ['#df202e', '#fc5855'],
    '--color-error-muted': 'var(--fj-wash)',
    '--color-success': ['#02853c', '#0fbd59'],
    '--color-success-muted': ['#e0f0e3', '#293a2c'],

    // A warning is the one thing the other two status hues cannot say: it must
    // read as neither an error nor a note, and both of those are already red. So
    // it is a fourth hue — 75°, the only colour on the page that is no code
    // colour — and the one that cannot sit at the others' lightness. A yellow
    // has no chroma left at 58%, and the shipped one bought its chroma at 79%,
    // where the icon went pale against the ground it stands on. 66% .138 is
    // where it is both amber and findable.
    '--color-warning': ['#c38406', '#f3a504'],
    '--color-warning-muted': ['#f5e9da', '#3f3321'],
  },
})
