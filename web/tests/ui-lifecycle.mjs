// Run with: node tests/ui-lifecycle.mjs (from web/). Uses installed build tools only.
import { build } from 'esbuild'
import { parse, compileScript } from '@vue/compiler-sfc'
import { readFile, mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const dir = await mkdtemp(join(tmpdir(), 'hm-ui-lifecycle-'))
const intervals = new Map()
let intervalId = 0
Object.assign(globalThis, {
  Document: class Document {}, ShadowRoot: class ShadowRoot {},
  HTMLElement: class HTMLElement {},
  localStorage: { getItem: key => key === 'hm.locale' ? 'en' : null, setItem() {}, removeItem() {} },
  window: { setTimeout, clearTimeout, setInterval: fn => { const id = ++intervalId; intervals.set(id, fn); return id }, clearInterval: id => intervals.delete(id) },
  document: { hidden: false, addEventListener() {}, removeEventListener() {}, activeElement: null, createElement: () => ({ content: {} }) },
  __polls: intervals,
})
try {
  const outfile = join(dir, 'cases.mjs')
  await build({
    entryPoints: [fileURLToPath(new URL('./ui-lifecycle.cases.ts', import.meta.url))], outfile,
    bundle: true, platform: 'node', format: 'esm', define: { 'import.meta.env.DEV': 'false' },
    plugins: [{ name: 'vue-sfc', setup(b) {
      b.onLoad({ filter: /\.vue$/ }, async ({ path }) => {
        const source = await readFile(path, 'utf8')
        const { descriptor } = parse(source, { filename: path })
        const script = compileScript(descriptor, { id: path, inlineTemplate: true })
        return { contents: script.content, loader: 'ts', resolveDir: dirname(path) }
      })
    } }],
  })
  await import(pathToFileURL(outfile).href)
} finally {
  await rm(dir, { recursive: true, force: true })
}
