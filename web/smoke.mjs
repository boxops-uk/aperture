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
// The page opens on the plan, which is the last thing the front end produces —
// so its presence says every phase before it ran.
await page.waitForSelector('.plan .steps li', { timeout: 15_000 })

check('the engine reports a version', /\d+\.\d+\.\d+/.test(await page.$eval('.status', (el) => el.textContent)))

// ---- the lowered view: the phase that needs a schema ----

await page.click('.tab:nth-child(3)')
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

await page.click('.tab:nth-child(1)')
await page.waitForSelector('.scroller tbody tr')
await type('.input', 'P where src.File P; P = 7 ~')

const tokens = (await page.$$eval('.scroller tbody tr', (trs) =>
  trs.map((tr) => [...tr.querySelectorAll('td')].map((td) => td.textContent)),
)).filter((row) => row[2] !== 'whitespace')

check(
  "the tokens are the lexer's, kind for kind",
  JSON.stringify(tokens.map((row) => [row[1], row[2], row[3]])) ===
    JSON.stringify([
      ['UId', 'variable', 'P'],
      ['Where', 'keyword', 'where'],
      ['QId', 'predicate', 'src.File'],
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

await page.click('.tab:nth-child(2)')
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

await type('.input', 'P where src.File P')
check('a supported query compiles clean', (await page.$$('.diagnostics li')).length === 0)

await page.click('.tab:nth-child(4)')
await page.waitForSelector('.plan .steps li')
check(
  'the plan is what the engine printed',
  (await texts('.plan pre')).some((text) => text.includes('src.File scan')),
)

// **The reorderer is the thing worth seeing.** Written in this order the join
// reads File second; the constraint on it makes it the cheaper place to start,
// and the plan says so by putting it first.
await type('.input', 'N where src.Module {file = F, name = N}; F = src.File P; P = "src/"..')
await page.waitForSelector('.plan .steps li')
const steps = await texts('.plan .steps pre')
check(
  'the reorderer moved the constrained predicate first',
  steps[0].includes('src.File') && steps[1].includes('src.Module'),
)
check(
  'a seek is told apart from a scan',
  (await texts('.plan .badge.seek')).length > 0,
)
check(
  'the plan carries the fingerprint a cursor would',
  /^[0-9a-f]{16}$/.test(await page.$eval('.plan .fingerprint', (el) => el.textContent)),
)

// A refused query has no plan at all, which is the rule the server runs under.
await type('.input', 'X where src.Nonesuch X')
check(
  'a refused query shows no plan',
  (await page.$$('.plan .steps li')).length === 0 && (await page.$$('.empty')).length === 1,
)

await page.click('.disclosure')
await page.waitForSelector('.editor.tall .input')
check('the schema lists what it declares', (await page.$$('.predicates li')).length >= 20)

// The schema pane is painted by the *schema* lexer, which has comments where
// sigla has none — and `schemas/code.sigla` is more comment than declaration.
check(
  'the schema is painted by its own lexer',
  await page.$$eval('.editor.tall .paint .tok', (ts) => {
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
await type('.editor.tall .input', 'schema src { predicate Nothing : string }')
check(
  'a query stops typechecking when its schema stops declaring it',
  (await texts('.diagnostics li')).some((text) => text.includes('reject/unknown-predicate')),
)

await browser.close()
await server.close()

if (problems.length) {
  console.error(`\n${problems.length} problem(s):\n${problems.join('\n')}`)
  process.exit(1)
}
console.log('\nthe demo runs.')
