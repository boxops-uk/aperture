import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { copyFileSync } from 'node:fs'
import { resolve } from 'node:path'

// The book lives in `website/content/`, one directory up and outside this
// package: the generated site and this one read the same files, so the dev
// server has to be allowed to serve from there.
export default defineConfig({
  base: process.env.SITE_BASE ?? '/',
  plugins: [
    react(),
    {
      // A page is a path, so a host that has never heard of `/storage` must
      // still answer with the application. GitHub Pages does that through
      // `404.html`, and this is the copy of it.
      name: 'fjord-spa-fallback',
      closeBundle() {
        const out = resolve(__dirname, 'dist')
        copyFileSync(resolve(out, 'index.html'), resolve(out, '404.html'))
      },
    },
  ],
  server: { fs: { allow: ['..'] } },
})
