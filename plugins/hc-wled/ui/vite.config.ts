// One ES module, everything inside it: the app shares nothing with a plugin's
// screens but the `host` it hands them. The screens were the app's own until
// WLED moved here; `#lib` and `$app/state` resolve to this package's own
// modules, which give the names they used over `host`.
import { defineConfig } from 'vite';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { fileURLToPath } from 'node:url';

const here = (p: string) => fileURLToPath(new URL(p, import.meta.url));

export default defineConfig({
	plugins: [svelte({ preprocess: vitePreprocess(), compilerOptions: { customElement: true } })],
	resolve: {
		alias: [
			{ find: /^#lib\//, replacement: here('./src/lib/') },
			{ find: /^\$app\/state$/, replacement: here('./src/lib/appState.svelte.ts') }
		]
	},
	build: {
		lib: { entry: 'src/index.ts', formats: ['es'], fileName: () => 'index.js' },
		outDir: 'dist',
		emptyOutDir: true,
		target: 'es2022',
		cssCodeSplit: false
	},
	test: { include: ['src/**/*.test.ts'] }
});
