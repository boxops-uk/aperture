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
import { existsSync, readdirSync } from 'node:fs'
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

const problems = []
page.on('console', (m) => m.type() === 'error' && problems.push(`console: ${m.text()}`))
page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`))

const check = (claim, ok) => {
  console.log(`${ok ? '  ok  ' : ' FAIL '} ${claim}`)
  if (!ok) problems.push(claim)
}
const settle = () => new Promise((resolve) => setTimeout(resolve, 250))
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
    const head = [...document.querySelectorAll('.section-head')].find(
      (head) => head.querySelector('.what')?.textContent === name,
    )
    if (!head) return false
    if (head.getAttribute('aria-expanded') !== 'true') head.click()
    return true
  }, name)
  if (!opened) throw new Error(`no section called ${name}`)
  await settle()
}

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

await page.goto(url, { waitUntil: 'networkidle0' })
// The page opens on the run, which is the last thing there is — so its presence
// says every phase before it ran too.
await page.waitForSelector('.run .transport', { timeout: 15_000 })

// **Plan and run at once.** The three columns are the point of the layout: what
// was typed, what the compiler made of it, and what the machine is doing.
// **Plan and run at once**, either side of a table of the database — which is
// the whole shape of the page: what the compiler decided, what the machine is
// doing about it, and the bytes both are about.
await page.waitForSelector('.data tr.section')
check(
  'the plan, the run and the database are on screen together',
  (await page.$$('.split .side')).length === 2 &&
    (await page.$$('.plan .steps li')).length > 0 &&
    (await page.$$('.run .transport')).length === 1 &&
    (await page.$$('.data tr.section')).length === 6,
)
check(
  'the split can be resized',
  (await page.$$('.grip[role="separator"]')).length === 1,
)

check('the engine reports a version', /\d+\.\d+\.\d+/.test(await page.$eval('.status', (el) => el.textContent)))

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
await type('.input', 'N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}')
for (let i = 0; i < 3; i++) await page.click('.run .transport button:nth-child(4)')
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
await type('.input', 'N where code.Decl {file = _, name = N, line = L}; L > 15')
await page.click('.run .transport button:nth-child(4)')
await settle()
check('a row read and dropped is marked as dropped', (await page.$$('.data tr.dropped')).length === 1)

// ---- the debugger: the machine, one transition at a time ----

// A query whose scan reads rows and drops them, which is the thing that is
// invisible everywhere except here.
await type('.input', 'N where code.Decl {file = _, name = N, line = L}; L > 15')
await page.waitForSelector('.run .transport')

const events = async () => {
  const seen = []
  const total = Number((await page.$eval('.run .count', (el) => el.textContent)).split('/')[1])
  for (let i = 0; i < total; i++) {
    seen.push(await page.$eval('.run .event .badge', (el) => el.textContent))
    if (i < total - 1) await page.click('.run .transport button:nth-child(4)')
  }
  return seen
}

const seen = await events()
check('the run steps through every transition', seen.length > 8)
check('a row read and dropped is shown as one', seen.includes('reject'))
check('a row answered is shown as one', seen.includes('yield'))
check('the run ends by saying so', seen.at(-1) === 'done')

// Stepping back is free, because the whole trace is already here.
await page.click('.run .transport button:nth-child(1)')
check(
  'stepping back to the start empties the registers',
  (await page.$$('.run .registers li')).length === 0,
)

// Step over: to the next row rather than the next transition.
await page.click('.run .transport button:nth-child(5)')
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
const transport = (label) =>
  page.evaluate((label) => {
    const button = [...document.querySelectorAll('.run .transport button')].find(
      (button) => button.textContent.trim() === label,
    )
    button.click()
  }, label)
const playLabel = () => page.$eval('.run .transport .play', (el) => el.textContent.trim())
const stepNow = async () => Number((await page.$eval('.run .count', (el) => el.textContent)).split('/')[0])

await page.click('.run .transport button:nth-child(1)')
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
await type('.input', 'P where code.File P; P = 7 ~')

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

await type('.input', 'P where code.File P')
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
await type('.input', 'N where code.Decl {file = F, name = N, line = _}; F = code.File P; P = "src/u"..')
await page.waitForSelector('.plan .steps li')
const steps = await texts('.plan .steps pre')
check(
  'the reorderer moved the constrained predicate first',
  steps[0].includes('code.File') && steps[1].includes('code.Decl'),
)
check(
  'a seek is told apart from a scan',
  (await texts('.plan .badge.seek')).length > 0,
)
check(
  'the plan carries the fingerprint a cursor would',
  /^[0-9a-f]{16}$/.test(await page.$eval('.plan .fingerprint', (el) => el.textContent)),
)

// The plan is not a description while a run is stepping: it is the thing being
// executed, and the step the machine is standing at says so.
await type('.input', 'N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}')
for (let i = 0; i < 2; i++) await page.click('.run .transport button:nth-child(4)')
await settle()
check('the plan lights the step the machine is standing at', (await page.$$('.plan .steps li.on')).length === 1)
check(
  'the plan says what each step has read so far',
  (await texts('.plan .badge.examined')).some((badge) => /\d+ read/.test(badge)),
)

// A refused query has no plan *and* no run, and both say so in their own words
// rather than showing an empty panel that reads like an answer of no rows.
await type('.input', 'X where code.Nonesuch X')
await page.waitForFunction(() =>
  [...document.querySelectorAll('.empty')].some((said) => said.textContent.includes('refused')),
)
check(
  'a refused query shows no plan and no run',
  (await page.$$('.plan .steps li')).length === 0 &&
    (await texts('.empty')).some((said) => said.includes('refused')) &&
    (await page.$$('.run .transport')).length === 0,
)

// The schema is a drawer: context rather than work, and the width it would
// take is the width the database table needs.
await page.click('.schema-open')
await page.waitForSelector('.drawer .editor.tall .input')
check('the schema lists what it declares', (await page.$$('.predicates li')).length === 6)

// The schema pane is painted by the *schema* lexer, which has comments where
// sigla has none — and `schemas/code.sigla` is more comment than declaration.
check(
  'the schema is painted by its own lexer',
  await page.$$eval('.drawer .editor.tall .paint .tok', (ts) => {
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
await type('.drawer .editor.tall .input', 'schema code { predicate Nothing : string }')
check(
  'a query stops typechecking when its schema stops declaring it',
  (await texts('.diagnostics li')).some((text) => text.includes('reject/unknown-predicate')),
)

// The drawer closes on Escape, because one that traps you is worse than a panel.
await page.keyboard.press('Escape')
await settle()
check('the drawer closes on escape', (await page.$$('.drawer')).length === 0)

await browser.close()
await server.close()

if (problems.length) {
  console.error(`\n${problems.length} problem(s):\n${problems.join('\n')}`)
  process.exit(1)
}
console.log('\nthe demo runs.')
