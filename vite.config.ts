import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';
import pkg from './package.json' with { type: 'json' };

export default defineConfig({
  plugins: [svelte()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  clearScreen: false,
  css: {
    // Vite 8 runs CSS through Lightning CSS by default, and it strips
    // backdrop-filter entirely — not a single such rule was left in the document,
    // and the glass turned into a flat fill. The UI renders in
    // WebView2 (a recent Chromium), so there is no reason to downlevel CSS.
    transformer: 'postcss',
  },
  server: {
    host: '127.0.0.1',
    port: 5343,
    strictPort: true,
    watch: {
      // Runtimes, data and downloaded sources live inside the project folder:
      // keeping them under the watcher is impossible — it fails with EBUSY on temp files.
      ignored: [
        '**/src-tauri/**',
        '**/runtime/**',
        '**/data/**',
        '**/models/**',
        '**/comfyui/**',
        // In a debug build st_dir is the sillytavern folder at the repo root: ST writes
        // data there atomically under its own temp names, not *.tmp.
        '**/sillytavern/**',
        '**/.st-research/**',
        '**/release/**',
        '**/memory/**',
        // Editors' atomic-write temp files: the watcher fails on them with EBUSY.
        '**/*.tmp',
        '**/*.tmpdir/**',
        '**/.*.tmpdir/**',
      ],
    },
  },
});
