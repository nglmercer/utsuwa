import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';
import { readFileSync } from 'fs';

const pkg = JSON.parse(readFileSync('./package.json', 'utf-8'));

export default defineConfig({
	plugins: [sveltekit(), tailwindcss()],
	define: {
		'import.meta.env.VITE_APP_VERSION': JSON.stringify(pkg.version),
		// True only for desktop builds: the Tauri CLI (which sets
		// TAURI_ENV_PLATFORM) or the native Rust host (UTSUWA_NATIVE=1).
		// Baked in at build time so routing decisions never depend on
		// runtime-injected globals.
		__IS_DESKTOP__: JSON.stringify(!!process.env.TAURI_ENV_PLATFORM || !!process.env.UTSUWA_NATIVE)
	},
	ssr: {
		noExternal: ['bits-ui']
	}
});
