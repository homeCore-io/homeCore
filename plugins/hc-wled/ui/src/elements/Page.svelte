<svelte:options customElement={{ tag: 'hc-wled-page', props: { host: { type: 'Object' } } }} />

<!-- WLED's plugin page: its controllers, what spans them, and its settings. -->
<script lang="ts">
	import type { Host } from '#lib/host.js';
	import WledControllers from '#lib/components/plugins/WledControllers.svelte';
	import { start } from './adopt.js';

	let { host: hc }: { host: Host } = $props();
	const me = $host();
	// Before the page draws: its parts read the host as they start.
	start(me, hc);
	$effect(() => start(me, hc));
</script>

{#if hc}<WledControllers />{/if}
