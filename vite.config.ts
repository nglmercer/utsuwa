import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';
import { readFileSync } from 'fs';

const pkg = JSON.parse(readFileSync('./package.json', 'utf-8'));

export default defineConfig({
	plugins: [sveltekit(), tailwindcss()],
	define: {
		'import.meta.env.VITE_APP_VERSION': JSON.stringify(pkg.version),
		// Native packaging hint. Runtime bridge detection in platform.ts remains
		// authoritative for dev WebViews, which may load Vite without this env.
		__IS_DESKTOP__: JSON.stringify(process.env.UTSUWA_NATIVE === '1')
	},
	ssr: {
		noExternal: ['bits-ui']
	}
});
