// **The demo, driven in a real browser.**
//
// The console program is the test, as it is for `clients/dotnet`: a unit test
// of this page would mock the one thing worth checking — that a WebAssembly
// module built from the engine loads under a browser's own loader and answers
// what the host suite says it answers. It builds, serves and drives the real
// bundle, and fails on any console error.
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

const browser = await puppeteer.launch({
  executablePath,
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
})
const page = await browser.newPage()

const problems = []
page.on('console', (m) => m.type() === 'error' && problems.push(`console: ${m.text()}`))
page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`))

const check = (claim, ok) => {
  console.log(`${ok ? '  ok  ' : ' FAIL '} ${claim}`)
  if (!ok) problems.push(claim)
}

await page.goto(url, { waitUntil: 'networkidle0' })
await page.waitForSelector('.scroller tbody tr', { timeout: 15_000 })

const rows = () =>
  page.$$eval('.scroller tbody tr', (trs) =>
    trs.map((tr) => [...tr.querySelectorAll('td')].map((td) => td.textContent)),
  )

check('the engine reports a version', /\d+\.\d+\.\d+/.test(await page.$eval('.status', (el) => el.textContent)))

// Typing is the whole demo: the tokens must follow the keystrokes.
await page.click('.input')
await page.keyboard.down('Control')
await page.keyboard.press('KeyA')
await page.keyboard.up('Control')
await page.keyboard.type('X where test.Count X; X = 7 ~')
await new Promise((resolve) => setTimeout(resolve, 250))

const typed = (await rows()).filter((row) => row[2] !== 'whitespace')
check(
  'the tokens are the lexer\'s, kind for kind',
  JSON.stringify(typed.map((row) => [row[1], row[2], row[3]])) ===
    JSON.stringify([
      ['UId', 'variable', 'X'],
      ['Where', 'keyword', 'where'],
      ['QId', 'predicate', 'test.Count'],
      ['UId', 'variable', 'X'],
      ['Semi', 'punctuation', ';'],
      ['UId', 'variable', 'X'],
      ['Eq', 'punctuation', '='],
      ['Nat', 'number', '7'],
      ['Error', 'error', '~'],
    ]),
)
check(
  'an unreadable byte is reported where it is',
  (await page.$$eval('.diagnostics li', (ls) => ls.map((l) => l.textContent)))
    .some((text) => text.includes('invalid token')),
)
check(
  'every class the page styles reaches the paint layer',
  new Set(await page.$$eval('.paint .tok', (ts) => ts.map((t) => t.className))).size >= 5,
)

// The parse tree is the second view, and the point of it is that a page can
// address the *shape* rather than a rendered string.
await page.click('.tab:nth-child(2)')
await page.waitForSelector('.tree li')
const kinds = await page.$$eval('.tree li .kind', (ks) => ks.map((k) => k.textContent))
check('the tree is the parser\'s, rule for rule', ['Root', 'Query', 'StmtList'].every((k) => kinds.includes(k)))
check('a recovered parse marks where it recovered', kinds.includes('Error'))

// Hovering a node highlights the source it covers: one span, two views.
await page.hover('.tree li:nth-child(4)')
await new Promise((resolve) => setTimeout(resolve, 150))
check(
  'hovering a node highlights the source it covers',
  (await page.$$('.paint .tok.on')).length > 0,
)

// A query the corpus calls supported must parse without a word of complaint.
await page.click('.input')
await page.keyboard.down('Control')
await page.keyboard.press('KeyA')
await page.keyboard.up('Control')
await page.keyboard.type('X where test.Foo {name = X}')
await new Promise((resolve) => setTimeout(resolve, 250))
check(
  'a supported query parses clean',
  (await page.$$('.diagnostics li')).length === 0 && (await page.$$('.tree li')).length > 5,
)

await browser.close()
await server.close()

if (problems.length) {
  console.error(`\n${problems.length} problem(s):\n${problems.join('\n')}`)
  process.exit(1)
}
console.log('\nthe demo runs.')
