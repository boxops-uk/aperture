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
await page.waitForSelector('.tokens tbody tr', { timeout: 15_000 })

const rows = () =>
  page.$$eval('.tokens tbody tr', (trs) =>
    trs.map((tr) => [...tr.querySelectorAll('td')].map((td) => td.textContent)),
  )

check('the engine reports a version', /\d+\.\d+\.\d+/.test(await page.$eval('.status', (el) => el.textContent)))

// Typing is the whole demo: the tokens must follow the keystrokes.
await page.click('.input')
await page.keyboard.down('Control')
await page.keyboard.press('KeyA')
await page.keyboard.up('Control')
await page.keyboard.type('where test.Count = 7 ~')
await new Promise((resolve) => setTimeout(resolve, 250))

const typed = (await rows()).filter((row) => row[2] !== 'whitespace')
check(
  'the tokens are the lexer\'s, kind for kind',
  JSON.stringify(typed.map((row) => [row[1], row[2], row[3]])) ===
    JSON.stringify([
      ['Where', 'keyword', 'where'],
      ['QId', 'predicate', 'test.Count'],
      ['Eq', 'punctuation', '='],
      ['Nat', 'number', '7'],
      ['Error', 'error', '~'],
    ]),
)
check(
  'an unreadable byte is reported where it is',
  (await page.$$eval('.diagnostics li', (ls) => ls.map((l) => l.textContent)))
    .some((text) => text.includes('invalid token') && text.includes('21')),
)
check(
  'every class the page styles reaches the paint layer',
  new Set(await page.$$eval('.paint .tok', (ts) => ts.map((t) => t.className))).size >= 5,
)

await browser.close()
await server.close()

if (problems.length) {
  console.error(`\n${problems.length} problem(s):\n${problems.join('\n')}`)
  process.exit(1)
}
console.log('\nthe demo runs.')
