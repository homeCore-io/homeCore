// One ES module, everything inside it: the app shares nothing with a plugin's
// screens but the `host` it hands them.
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
	plugins: [svelte({ compilerOptions: { customElement: true } })],
	build: {
		lib: { entry: 'src/index.js', formats: ['es'], fileName: () => 'index.js' },
		outDir: 'dist',
		emptyOutDir: true,
		target: 'es2022',
		cssCodeSplit: false
	}
});
