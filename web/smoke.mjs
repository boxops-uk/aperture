// **The demo, driven in a real browser.**
//
// The console program is the test, as it is for `clients/dotnet`: a unit test of
// this page would mock the one thing worth checking — that a WebAssembly module
// built from the engine loads under a browser's own loader and answers what the
// host suite says it answers. It builds, serves and drives the real bundle, and
// fails on any console error.
//
// It needs a Chrome to drive, from `$CHROME` or puppeteer's cache, and says so
// rather than passing vacuously when there is none.
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { preview, build } from 'vite'
import puppeteer from 'puppeteer-core'

const CACHE = `${process.env.HOME}/.cache/puppeteer/chrome`

function chrome() {
  if (process.env.CHROME) return process.env.CHROME
  if (!existsSync(CACHE)) return null
  for (const version of readdirSync(CACHE)) {
    const path = `${CACHE}/${version}/chrome-linux64/chrome`
    if (existsSync(path)) return path
  }
  return null
}

const executablePath = chrome()
if (!executablePath) {
  console.error('no browser to drive: set $CHROME, or `npx puppeteer browsers install chrome`')
  process.exit(2)
}

await build({ logLevel: 'warn' })
const server = await preview({ preview: { port: 4173, strictPort: true } })
const url = server.resolvedUrls.local[0]

// Launching flakes in a container often enough to matter — crashpad probing
// `/sys/devices/system/cpu/.../cpufreq` that is not there — and a flaky check is
// one people learn to re-run rather than read. Three tries, then it is real.
async function launch(attempts = 3) {
  for (let attempt = 1; ; attempt++) {
    try {
      return await puppeteer.launch({
        executablePath,
        args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu'],
      })
    } catch (error) {
      if (attempt >= attempts) throw error
      console.log(`  ..   the browser did not start (attempt ${attempt}); trying again`)
      await new Promise((resolve) => setTimeout(resolve, 500))
    }
  }
}

const browser = await launch()
const page = await browser.newPage()
// A desktop viewport, deliberately: the shell collapses the side nav into a
// drawer and overlays the workbench's panel below 1024px, so the default
// 800×600 window would be checking the phone layout by accident.
await page.setViewport({ width: 1440, height: 900 })

const problems = []
page.on('console', (m) => m.type() === 'error' && problems.push(`console: ${m.text()}`))
page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`))

const check = (claim, ok, detail = '') => {
  console.log(`${ok ? '  ok  ' : ' FAIL '} ${claim}${ok || !detail ? '' : ` — ${detail}`}`)
  if (!ok) problems.push(claim + (detail ? ` — ${detail}` : ''))
}
const settle = () => new Promise((resolve) => setTimeout(resolve, 250))
// `.editor .input`, never a bare `.input`: the design system uses `input` as a
// variant class, so the sample select answers a bare one first.
const type = async (selector, text) => {
  await page.click(selector)
  await page.keyboard.down('Control')
  await page.keyboard.press('KeyA')
  await page.keyboard.up('Control')
  await page.keyboard.type(text)
  await settle()
}
const texts = (selector) => page.$$eval(selector, (els) => els.map((el) => el.textContent))
/** Open one section of the left-hand accordion, by its name. */
const openSection = async (name) => {
  const opened = await page.evaluate((name) => {
    const head = [...document.querySelectorAll('.astryx-collapsible-trigger')].find((head) =>
      head.textContent?.toLowerCase().startsWith(name.toLowerCase()),
    )
    if (!head) return false
    if (head.getAttribute('aria-expanded') !== 'true') head.click()
    return true
  }, name)
  if (!opened) throw new Error(`no section called ${name}`)
  await settle()
}

/** Press one of the transport's buttons, by the word on it. */
const transport = (label) =>
  page.evaluate((label) => {
    const button = [...document.querySelectorAll('.transport button')].find(
      (button) => button.textContent?.trim() === label,
    )
    if (!button) throw new Error(`no transport button called ${label}`)
    if (button.disabled) return 'disabled'
    button.click()
    return 'clicked'
  }, label)

/** Unfold every predicate in the database table, whatever the run folded. */
const openEveryPredicate = async () => {
  await page.evaluate(() => {
    for (const head of document.querySelectorAll('.data tr.section button'))
      if (head.getAttribute('aria-expanded') !== 'true') head.click()
  })
  await settle()
}

/** Which predicates the database table is showing rows for, in order. */
const unfolded = () =>
  page.$$eval('.data tr.section button', (heads) =>
    heads
      .filter((head) => head.getAttribute('aria-expanded') === 'true')
      .map((head) => head.querySelector('.name').textContent),
  )

const nodeRow = async (kind) =>
  (
    await page.evaluateHandle(
      (kind) =>
        [...document.querySelectorAll('.tree li')].find(
          (li) => li.querySelector('.kind')?.textContent === kind,
        ),
      kind,
    )
  ).asElement()

await page.goto(`${url}playground`, { waitUntil: 'networkidle0' })
// The page opens on the run, which is the last thing there is — so its presence
// says every phase before it ran too.
await page.waitForSelector('.transport', { timeout: 15_000 })

// **Plan and run at once.** The three columns are the point of the layout: what
// was typed, what the compiler made of it, and what the machine is doing.
// **Plan and run at once**, either side of a table of the database — which is
// the whole shape of the page: what the compiler decided, what the machine is
// doing about it, and the bytes both are about.
await page.waitForSelector('.data tr.section')
check(
  'the plan, the run and the database are on screen together',
  (await page.$$('.astryx-layout-panel')).length >= 2 &&
    (await page.$$('.plan .steps li')).length > 0 &&
    (await page.$$('.transport')).length === 1 &&
    (await page.$$('.data tr.section')).length === 6,
)
check('the split can be resized', (await page.$$('.astryx-resize-handle')).length >= 1)

check(
  'the engine reports a version',
  /\d+\.\d+\.\d+/.test(await page.$eval('.astryx-toolbar', (el) => el.textContent)),
)

// ---- the database, and the range a scan walks across it ----

await openEveryPredicate()
check(
  'the database shows every stored row',
  (await page.$$('.data tbody tr')).length >= 36 + (await page.$$('.data tr.section')).length,
)
check(
  'a stored key is shown as bytes and as a fact',
  await page.$$eval('.data tbody tr', (rows) =>
    rows.some((row) => {
      const bytes = row.querySelector('.bytes')?.textContent ?? ''
      const decoded = row.querySelector('.decoded')?.textContent ?? ''
      return /^[0-9a-f]{8,}$/.test(bytes) && decoded.includes('{')
    }),
  ),
)

// A join: the inner level seeks, so its range covers exactly the rows of one
// file — a band across the table rather than the whole predicate.
await type('.editor .input', 'N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}')
for (let i = 0; i < 3; i++) await transport('▶')
await settle()

check(
  'the range being scanned is shown as bytes',
  /^[0-9a-f]+$/.test((await texts('.data h2 .range code'))[0] ?? ''),
)
const within = (await page.$$('.data tr.within')).length
check('the range shades the rows inside it', within >= 2 && within <= 4)
check('the row the machine holds is marked', (await page.$$('.data tr.held')).length >= 1)
check(
  'the bytes the seek pinned are marked off from the ones it walks',
  (await page.$$('.data .pinned')).length >= 2,
)

// A join stands in two predicates at once, and they are not neighbours: the
// table folds the four it is not about and leaves *both* of the two it is.
const open = await unfolded()
check(
  'stepping folds the predicates the step is not about',
  open.length === 2 && open.includes('code.File') && open.includes('code.Decl'),
  open.join(' '),
)

// Folded by the run, not locked by it.
await page.evaluate(() => {
  const head = [...document.querySelectorAll('.data tr.section button')].find(
    (head) => head.querySelector('.name').textContent === 'code.Span',
  )
  head.click()
})
await settle()
check('a predicate opened by hand stays open', (await unfolded()).includes('code.Span'))

// A scan with a residual: the rows it reads and drops go red.
await type('.editor .input', 'N where code.Decl {file = _, name = N, line = L}; L > 15')
await transport('▶')
await settle()
check('a row read and dropped is marked as dropped', (await page.$$('.data tr.dropped')).length === 1)

// **An empty box is not a mistake.** Every phase has something to say about the
// empty string, and none of it is about anything the reader did.
await type('.editor .input', ' ')
await page.keyboard.press('Backspace')
await settle()
check('an empty query reports nothing', (await page.$$('.diagnostics li')).length === 0)
check(
  'an empty query folds the whole database',
  (await unfolded()).length === 0 && (await page.$$('.data tr.section')).length === 6,
)
check(
  'an empty query leaves every view saying so',
  (await texts('.empty')).filter((said) => said.includes('yet')).length >= 3,
)

// ---- the debugger: the machine, one transition at a time ----

// A query whose scan reads rows and drops them, which is the thing that is
// invisible everywhere except here.
await type('.editor .input', 'N where code.Decl {file = _, name = N, line = L}; L > 15')
await page.waitForSelector('.transport')

const events = async () => {
  const seen = []
  const total = Number((await page.$eval('.transport .count', (el) => el.textContent)).split('/')[1])
  for (let i = 0; i < total; i++) {
    seen.push(await page.$eval('.run .event .badge', (el) => el.textContent))
    if (i < total - 1) await transport('▶')
  }
  return seen
}

const seen = await events()
check('the run steps through every transition', seen.length > 8)
check('a row read and dropped is shown as one', seen.includes('reject'))
check('a row answered is shown as one', seen.includes('yield'))
check('the run ends by saying so', seen.at(-1) === 'done')

// Stepping back is free, because the whole trace is already here.
await transport('|◀ start')
check(
  'stepping back to the start empties the registers',
  (await page.$$('.run .registers li')).length === 0,
)

// Step over: to the next row rather than the next transition.
await transport('row ▶')
check(
  'step over lands on a row',
  (await page.$eval('.run .event .badge', (el) => el.textContent)) === 'yield',
)
check(
  'a register holds the row the answer came from',
  (await texts('.run .registers li')).some((row) => row.includes('code.Decl#')),
)
check(
  'the rows so far are the rows yielded so far',
  (await page.$$('.run .yielded li')).length === 1,
)

// **Play, and then a hand on the controls.** A run that keeps advancing under
// someone who just stepped back is fighting them for the play head, so any
// navigation stops it — and the end of the run stops it too, rather than leaving
// a button that says "pause" and takes two clicks to start again.
const playLabel = () =>
  page.evaluate(() =>
    [...document.querySelectorAll('.transport button')]
      .map((button) => button.textContent.trim())
      .find((label) => label === 'play' || label === 'pause'),
  )
const stepNow = async () =>
  Number((await page.$eval('.transport .count', (el) => el.textContent)).split('/')[0])

await transport('|◀ start')
await transport('play')
await new Promise((resolve) => setTimeout(resolve, 700))
const playedTo = await stepNow()
check('play advances the run on its own', playedTo > 1 && (await playLabel()) === 'pause')

await transport('◀')
await new Promise((resolve) => setTimeout(resolve, 600))
check(
  'navigating while playing stops the run',
  (await playLabel()) === 'play' && (await stepNow()) === playedTo - 1,
)

await transport('end ▶|')
await settle()
check('the end of the run stops the run', (await playLabel()) === 'play')
await transport('play')
await settle()
check('play from the end starts again from the start', (await stepNow()) < 3)
await transport('pause')

// ---- the lowered view: the phase that needs a schema ----

await openSection('lowered')
await page.waitForSelector('.lowered li')
check(
  'the query is typed against the schema',
  (await texts('.lowered .ty')).some((ty) => ty === 'string'),
)
check(
  'every name in the view resolved',
  !(await texts('.lowered li')).some((row) => row.includes('<unresolved>')),
)

// ---- the tokens: what the lexer says, on every keystroke ----

await openSection('tokens')
await page.waitForSelector('.scroller tbody tr')
await type('.editor .input', 'P where code.File P; P = 7 ~')

const tokens = (await page.$$eval('.scroller tbody tr', (trs) =>
  trs.map((tr) => [...tr.querySelectorAll('td')].map((td) => td.textContent)),
)).filter((row) => row[2] !== 'whitespace')

check(
  "the tokens are the lexer's, kind for kind",
  JSON.stringify(tokens.map((row) => [row[1], row[2], row[3]])) ===
    JSON.stringify([
      ['UId', 'variable', 'P'],
      ['Where', 'keyword', 'where'],
      ['QId', 'predicate', 'code.File'],
      ['UId', 'variable', 'P'],
      ['Semi', 'punctuation', ';'],
      ['UId', 'variable', 'P'],
      ['Eq', 'punctuation', '='],
      ['Nat', 'number', '7'],
      ['Error', 'error', '~'],
    ]),
)
check(
  'an unreadable byte is reported where it is',
  (await texts('.diagnostics li')).some((text) => text.includes('invalid token')),
)
check(
  'every class the page styles reaches the paint layer',
  new Set(await page.$$eval('.paint .tok', (ts) => ts.map((t) => t.className))).size >= 5,
)

// ---- the parse tree: the shape, and how it is highlighted ----

await openSection('parse tree')
await page.waitForSelector('.tree li')
const kinds = await texts('.tree li .kind')
check("the tree is the parser's, rule for rule", ['Root', 'Query', 'StmtList'].every((k) => kinds.includes(k)))
check('a recovered parse marks where it recovered', kinds.includes('Error'))

await (await nodeRow('StmtList')).hover()
await settle()
check('hovering a node highlights the source it covers', (await page.$$('.paint .tok.on')).length > 0)
check(
  'hovering a node leaves its ancestors alone',
  await page.$$eval('.tree li.on .kind', (ks) => {
    const lit = ks.map((k) => k.textContent)
    return lit.length > 1 && !lit.includes('Root') && !lit.includes('Query')
  }),
)

// The chain `ImplicitBindStmt → Pattern → Sum → Fact → FactPattern` all cover
// exactly the same bytes, so nothing comparing *spans* can tell them apart. The
// highlight is by node, and this is the assertion that says so.
await (await nodeRow('FactPattern')).hover()
await settle()
check(
  'a same-span ancestor stays dark',
  await page.$$eval('.tree li.on .kind', (ks) => {
    const lit = ks.map((k) => k.textContent)
    return lit[0] === 'FactPattern' && !lit.includes('Fact') && !lit.includes('ImplicitBindStmt')
  }),
)

// ---- a clean query, and then the plan it compiles to ----

await type('.editor .input', 'P where code.File P')
check('a supported query compiles clean', (await page.$$('.diagnostics li')).length === 0)

await openSection('plan')
await page.waitForSelector('.plan .steps li')
check(
  'the plan is what the engine printed',
  (await texts('.plan pre')).some((text) => text.includes('code.File scan')),
)

// **The reorderer is the thing worth seeing.** Written in this order the join
// reads File second; the constraint on it makes it the cheaper place to start,
// and the plan says so by putting it first.
await type('.editor .input', 'N where code.Decl {file = F, name = N, line = _}; F = code.File P; P = "src/u"..')
await page.waitForSelector('.plan .steps li')
const steps = await texts('.plan .steps pre')
check(
  'the reorderer moved the constrained predicate first',
  steps[0].includes('code.File') && steps[1].includes('code.Decl'),
)
check(
  'a seek is told apart from a scan',
  (await texts('.plan .steps .astryx-token')).some((badge) => badge.trim() === 'seek'),
)
check(
  'the plan carries the fingerprint a cursor would',
  /^[0-9a-f]{16}$/.test(await page.$eval('.plan .fingerprint', (el) => el.textContent)),
)

// The plan is not a description while a run is stepping: it is the thing being
// executed, and the step the machine is standing at says so.
await type('.editor .input', 'N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}')
for (let i = 0; i < 2; i++) await transport('▶')
await settle()
check('the plan lights the step the machine is standing at', (await page.$$('.plan .steps li.on')).length === 1)
check(
  'the plan says what each step has read so far',
  (await texts('.plan .badge.examined')).some((badge) => /\d+ read/.test(badge)),
)

// A refused query has no plan *and* no run, and both say so in their own words
// rather than showing an empty panel that reads like an answer of no rows.
await type('.editor .input', 'X where code.Nonesuch X')
await page.waitForFunction(() =>
  [...document.querySelectorAll('.empty')].some((said) => said.textContent.includes('refused')),
)
check(
  'a refused query shows no plan and no run',
  (await page.$$('.plan .steps li')).length === 0 &&
    (await texts('.empty')).some((said) => said.includes('refused')) &&
    (await page.$$('.transport')).length === 0,
)

// The schema is a drawer: context rather than work, and the width it would
// take is the width the database table needs.
await page.click('[data-testid="schema"]')
await page.waitForSelector('dialog .editor.tall .input')
check('the schema lists what it declares', (await page.$$('.predicates li')).length === 6)

// The schema pane is painted by the *schema* lexer, which has comments where
// sigla has none — and `schemas/code.sigla` is more comment than declaration.
check(
  'the schema is painted by its own lexer',
  await page.$$eval('dialog .editor.tall .paint .tok', (ts) => {
    const classes = new Set(ts.map((t) => t.className))
    return (
      classes.has('tok tok-comment') &&
      classes.has('tok tok-keyword') &&
      ts.map((t) => t.textContent).join('').includes('predicate File : string')
    )
  }),
)

// Editing the schema recompiles the query, which is the whole point of the page
// holding one: the same schema the engine resolves names against.
await type('dialog .editor.tall .input', 'schema code { predicate Nothing : string }')
check(
  'a query stops typechecking when its schema stops declaring it',
  (await texts('.diagnostics li')).some((text) => text.includes('reject/unknown-predicate')),
)

// The drawer closes on Escape, because one that traps you is worse than a panel.
await page.keyboard.press('Escape')
await settle()
check('the drawer closes on escape', (await page.$$('dialog[open]')).length === 0)


// ---------------------------------------------------------------- the book --
//
// The pages are `website/content/`, parsed here rather than copied, and the
// demos in them are this same engine. Both halves are checked: that every page
// in the reading order renders, and that a demo on one of them runs.

const openPage = async (title) => {
  const found = await page.evaluate((title) => {
    const link = [...document.querySelectorAll('.astryx-side-nav-item')].find(
      (a) => a.textContent?.trim() === title,
    )
    if (!link) return false
    link.click()
    return true
  }, title)
  if (!found) throw new Error(`no page called ${title} in the nav`)
  await settle()
}

await page.goto(url, { waitUntil: 'networkidle0' })
await page.waitForSelector('[data-testid="prose"] h1')

check(
  'the site opens on the book',
  (await page.$eval('[data-testid="prose"] h1', (el) => el.textContent)) === 'Fjord DB',
)
check(
  'the reading order is the one the generator publishes',
  (await page.$$('.astryx-side-nav-item')).length === 21,
)

// A page is a route. If any of these were a document load the marker would be
// gone — and with it the engine, which is the whole reason this is one page.
await page.evaluate(() => {
  window.__oneApplication = true
})

const titles = await page.$$eval('.astryx-side-nav-item', (links) =>
  links.map((link) => link.textContent?.trim()),
)
const broken = []
for (const title of titles) {
  await openPage(title)
  // A page of the book renders its title; the workbench renders its transport,
  // because it is an application and has no page heading.
  const alive = await page.evaluate(
    () =>
      Boolean(document.querySelector('[data-testid="prose"] h1')?.textContent?.trim()) ||
      Boolean(document.querySelector('.transport')),
  )
  if (!alive) broken.push(title)
}
check('every page in the reading order renders', broken.length === 0, broken.join(', '))
check('a page is a route, not a page load', await page.evaluate(() => window.__oneApplication === true))

await openPage('Storage model')
check(
  'the page a reader is on is the page the tab says',
  (await page.title()) === 'Storage model · Fjord DB',
)
check(
  "the table of contents is the page's own headings",
  (await page.$$('.astryx-outline a')).length > 5,
)
check(
  'the pager follows the reading order',
  (await page.$eval('[data-testid="pager-next"]', (el) => el.textContent)).includes('Executor'),
)

// The demo on this page is the database, and it is the real one: 36 facts,
// written through the same encoder a client writes with.
await page.waitForSelector('.demo .data table', { timeout: 20_000 })
check('a demo runs the engine in the page', (await page.$$('.demo .data tbody tr')).length > 6)
check(
  'the demo carries its query to the workbench',
  (await page.$eval('[data-testid="demo-open"]', (el) => el.getAttribute('href'))).includes(
    'q=P+where+code.File',
  ),
)

// Editing a demo recompiles it, because there is nothing else it could do.
await type('.demo .editor .input', 'P where code.Nonesuch P')
check(
  'editing a demo recompiles it',
  (await texts('.demo .diagnostics li')).some((said) => said.includes('reject/unknown-predicate')),
)

// A page with a demo on it has the module, and then the static blocks stop
// being painted by regular expressions and are painted by the lexer that
// compiles them — token kinds the fallback rules do not have.
await openPage('sigla query language')
await page.waitForSelector('.demo .scroller tbody tr', { timeout: 20_000 })
// The highlights are CSS ranges rather than elements, so the block says which
// painter it got: `engine` once the module a demo pulled in has landed.
check(
  "a sigla block is painted by the engine's own lexer",
  (await page.$$('[data-painted="engine"]')).length > 0,
)
check(
  'a demo of the lexer is the lexer',
  (await texts('.demo .scroller tbody tr td')).includes('Where'),
)

// Search is over every heading of every page, built from the same pages.
await page.keyboard.press('Escape')
await page.click('[data-testid="search"]')
await page.waitForSelector('dialog input')
await page.keyboard.type('marker table')
await settle()
check(
  'search finds a heading',
  (await texts('.astryx-command-palette-item')).some((hit) => hit.includes('marker table')),
)
// Down first: the palette highlights nothing until a key or a pointer says so,
// and Enter on nothing is nothing.
await page.keyboard.press('ArrowDown')
await page.keyboard.press('Enter')
await settle()
check(
  'a search hit lands on the heading it named',
  await page.evaluate(() => window.location.hash === '#the-marker-table'),
)

// The theme is a choice, and a choice that is not remembered is not one. The
// design system paints through tokens rather than a root attribute, so what is
// checked is the paint and the memory of the choice.
// The shell paints the page, not the body: the body is transparent and the
// design system's surfaces are what a reader actually sees change.
const paper = () =>
  page.evaluate(() => getComputedStyle(document.querySelector('.astryx-app-shell')).backgroundColor)
const before = await paper()
await page.click('[data-testid="mode"]')
await settle()
const chosen = await page.evaluate(() => localStorage.getItem('fjord-theme'))
check(
  'the theme toggle chooses a theme',
  (chosen === 'dark' || chosen === 'light') && (await paper()) !== before,
)
await page.reload({ waitUntil: 'networkidle0' })
await settle()
check(
  'the theme sticks across a reload',
  (await page.evaluate(() => localStorage.getItem('fjord-theme'))) === chosen &&
    (await paper()) !== before,
)

// **One book, two renderers.** The generated site parses these pages in Python
// and this one parses them in TypeScript, and a dialect that drifts between them
// is a page that reads differently depending on which copy you found. Compared
// per page, and only when `website/site/` has been built — the comparison is
// worth having and is not worth failing the check for being absent.
const generated = new URL('../website/site/', import.meta.url)
if (existsSync(new URL('index.html', generated))) {
  const order = JSON.parse(readFileSync(new URL('../website/nav.json', import.meta.url), 'utf8'))
    .groups.flatMap((group) => group.pages.map((entry) => entry.slug))
  const count = (text, needle) => text.split(needle).length - 1
  const drift = []

  for (const slug of order) {
    const html = readFileSync(new URL(`${slug}.html`, generated), 'utf8')
    await page.goto(`${url}${slug === 'index' ? '' : slug}`, { waitUntil: 'networkidle0' })
    await page.waitForSelector('[data-testid="prose"] h1')
    const here = await page.evaluate(() => {
      // A demo has elements of its own — a table of rows, a schema in a code
      // block — and they are the workbench's, not the page's.
      const prose = (selector) =>
        [...document.querySelectorAll(`[data-testid="prose"] ${selector}`)].filter(
          (el) => !el.closest('.demo'),
        ).length
      return {
        h2: prose('h2'),
        h3: prose('h3'),
        tables: prose('table.astryx-table'),
        code: prose('pre.astryx-codeblock'),
        demos: document.querySelectorAll('[data-testid="prose"] .demo').length,
        callouts: prose('.astryx-banner'),
      }
    })
    const there = {
      h2: count(html, '<h2 id='),
      h3: count(html, '<h3 id='),
      tables: count(html, '<div class="table-wrap">'),
      code: count(html, '<figure class="code">'),
      demos: count(html, '<figure class="code demo">'),
      callouts: count(html, '<aside class="callout'),
    }
    for (const key of Object.keys(there)) {
      if (here[key] !== there[key]) drift.push(`${slug}: ${key} ${here[key]} vs ${there[key]}`)
    }
  }

  check('the two renderers agree, page for page', drift.length === 0, drift.slice(0, 6).join('; '))
} else {
  console.log('  ..   website/site/ is not built — skipping the two-renderer comparison')
}

// A path this site has never heard of is still this site.
await page.goto(`${url}nonesuch`, { waitUntil: 'networkidle0' })
check(
  'an unknown page says so rather than breaking',
  (await page.$eval('h1', (el) => el.textContent)) === 'Not a page',
)

await browser.close()
await server.close()

if (problems.length) {
  console.error(`\n${problems.length} problem(s):\n${problems.join('\n')}`)
  process.exit(1)
}
console.log('\nthe demo runs.')
