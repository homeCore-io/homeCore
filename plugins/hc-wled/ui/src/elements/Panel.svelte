<svelte:options
	customElement={{
		tag: 'hc-wled-panel',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A WLED controller's panel: its look, segments, presets, playlists and
     the controller itself. -->
<script lang="ts">
	import type { Host } from '#lib/host.js';
	import { devices } from '#lib/devices.svelte.js';
	import WledPanel from '#lib/components/wled/WledPanel.svelte';
	import { start } from './adopt.js';

	let { host: hc, deviceId }: { host: Host; deviceId?: string } = $props();
	const me = $host();
	start(me, hc);
	$effect(() => start(me, hc));

	const device = $derived(deviceId ? devices.byId[deviceId] : undefined);
	// Offline, or someone who may only look: every control is still.
	const disabled = $derived(
		!device?.available || !(hc?.me.can('devices:command') || hc?.me.can('devices:write'))
	);
</script>

{#if device}<WledPanel {device} {disabled} />{/if}
