import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

// `build` is the native adapter output. Do not index a stale native build
// after a normal adapter-auto build has run in the same worktree.
const candidates =
	process.env.UTSUWA_NATIVE === '1' ? ['build', '.vercel/output/static'] : ['.vercel/output/static'];
const site = candidates.find((directory) => existsSync(join(directory, 'index.html')));

if (!site) {
	console.log('Pagefind indexing skipped (no static HTML found)');
	process.exit(0);
}

const executable = process.platform === 'win32' ? 'pagefind.cmd' : 'pagefind';
const result = spawnSync(executable, ['--site', site], { stdio: 'inherit' });

if (result.error) {
	console.error(`Pagefind failed to start: ${result.error.message}`);
	process.exit(1);
}

process.exit(result.status ?? 1);
