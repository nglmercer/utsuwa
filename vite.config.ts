import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { createLogger, defineConfig } from 'vite';
import { readFileSync } from 'fs';

const pkg = JSON.parse(readFileSync('./package.json', 'utf-8'));
const logger = createLogger();
const defaultWarn = logger.warn.bind(logger);
logger.warn = (message, options) => {
	// @threlte/xr 1.6.1 ships an unused Mesh import in its touch-controls
	// SSR module. Keep other dependency and application warnings visible.
	if (
		message.includes('"Mesh" is imported from external module "three"') &&
		message.includes('@threlte/xr/dist/plugins/touchControls/setup.svelte.js')
	) {
		return;
	}

	// onnxruntime-web is a prebuilt dependency whose minified worker contains
	// eval; this is not application-authored code.
	if (message.includes('onnxruntime-web') && message.includes('Use of eval in')) return;

	defaultWarn(message, options);
};

export default defineConfig({
	customLogger: logger,
	plugins: [sveltekit(), tailwindcss()],
	define: {
		'import.meta.env.VITE_APP_VERSION': JSON.stringify(pkg.version),
		// Native packaging hint. Runtime bridge detection in platform.ts remains
		// authoritative for dev WebViews, which may load Vite without this env.
		__IS_DESKTOP__: JSON.stringify(process.env.UTSUWA_NATIVE === '1')
	},
	ssr: {
		noExternal: ['bits-ui']
	},
	build: {
		// The embedding runtime is loaded only when semantic memory is enabled;
		// its lazy chunk is intentionally larger than the default Rollup budget.
		chunkSizeWarningLimit: 900
	}
});
