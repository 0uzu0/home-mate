// Run with: node tests/audit-wave5.mjs (from web/). Uses installed build tools only.
import { build } from 'esbuild'
import { parse, compileScript } from '@vue/compiler-sfc'
import { readFile, mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const dir = await mkdtemp(join(tmpdir(), 'hm-audit-wave5-'))
const intervals = new Map()
const storage = new Map([['hm.locale', 'en']])
const local = {
  getItem: (key) => storage.get(key) ?? null,
  setItem: (key, value) => storage.set(key, String(value)),
  removeItem: (key) => storage.delete(key),
}
let intervalId = 0
const historyStub = { state: {}, replaceState() {}, pushState() {}, go() {} }
const locationStub = { pathname: '/', search: '', hash: '', href: 'http://localhost/' }
Object.assign(globalThis, {
  Document: class Document {},
  ShadowRoot: class ShadowRoot {},
  HTMLElement: class HTMLElement {},
  localStorage: local,
  history: historyStub,
  location: locationStub,
  window: {
    localStorage: local,
    history: historyStub,
    location: locationStub,
    setTimeout,
    clearTimeout,
    setInterval: (fn) => {
      const id = ++intervalId
      intervals.set(id, fn)
      return id
    },
    clearInterval: (id) => intervals.delete(id),
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    addEventListener() {},
    removeEventListener() {},
  },
  document: {
    hidden: false,
    documentElement: { lang: 'en', dataset: {} },
    title: '',
    addEventListener() {},
    removeEventListener() {},
    activeElement: null,
    querySelector: () => null,
    createElement: () => ({ content: {} }),
    scripts: [],
  },
  __polls: intervals,
})
try {
  const outfile = join(dir, 'cases.mjs')
  await build({
    entryPoints: [fileURLToPath(new URL('./audit-wave5.cases.ts', import.meta.url))],
    outfile,
    bundle: true,
    platform: 'node',
    format: 'esm',
    define: { 'import.meta.env.DEV': 'false' },
    plugins: [
      {
        name: 'vue-sfc',
        setup(b) {
          b.onLoad({ filter: /\.vue$/ }, async ({ path }) => {
            const source = await readFile(path, 'utf8')
            const { descriptor } = parse(source, { filename: path })
            const script = compileScript(descriptor, { id: path, inlineTemplate: true })
            return { contents: script.content, loader: 'ts', resolveDir: dirname(path) }
          })
        },
      },
    ],
  })
  await import(pathToFileURL(outfile).href)
} finally {
  await rm(dir, { recursive: true, force: true })
}
