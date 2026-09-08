// Keep native frontend commands cross-platform without requiring developers
// to remember the Vite compile-time flag. Runtime bridge detection still owns
// correctness for `cargo run -- --dev` when a plain `pnpm dev` is used.
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { pathToFileURL } from 'node:url';

process.env.UTSUWA_NATIVE = '1';

// Vite's package exports intentionally hide its CLI subpath, so resolve the
// installed package first and import the executable by absolute file URL.
const require = createRequire(import.meta.url);
const vitePackage = require.resolve('vite/package.json');
await import(pathToFileURL(join(dirname(vitePackage), 'bin/vite.js')).href);
