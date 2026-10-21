<svelte:options
	customElement={{
		tag: 'hc-caseta-scenes',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Caséta scenes", a card you add: the bridge's scenes you chose, in your
     order; a tap runs one. None is lit — the bridge doesn't say which is on. -->
<script>
	import { adopt } from './lib/lutron.js';

	let { host: hc, form = 'standard', config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const all = $derived(
		devices
			.filter((d) => d.device_type === 'scene')
			.sort((x, y) => hc.devices.label(x).localeCompare(hc.devices.label(y)))
	);
	const chosen = $derived(
		Array.isArray(config?.scenes) && config.scenes.length
			? config.scenes.map((id) => all.find((s) => s.device_id === id)).filter(Boolean)
			: all.slice(0, 6)
	);
	const title = $derived(config?.name || 'Caséta scenes');
	const canRun = $derived(!!hc?.me.can('devices:command'));

	let busy = $state('');
	let error = $state('');
	async function run(s) {
		busy = s.device_id;
		const r = await hc.command(s.device_id, { action: 'activate' });
		error = r.ok ? '' : r.error;
		setTimeout(() => (busy = ''), 800);
	}
	const page = () => hc.ui.navigate(`/settings/plugins/${encodeURIComponent(hc.pluginId)}`);
</script>

<div class="hc-tile card {form}">
	<div class="top">
		<span class="hc-tile-name">{title}</span>
		{#if form !== 'compact'}<button class="link" onclick={page}>Caséta ›</button>{/if}
	</div>
	{#if chosen.length}
		<div class="scenes" class:row={form === 'compact'}>
			{#each chosen as s (s.device_id)}
				<button
					class="scene"
					class:busy={busy === s.device_id}
					disabled={!canRun || s.available === false}
					onclick={() => run(s)}>{hc.devices.label(s)}</button
				>
			{/each}
		</div>
		{#if form !== 'compact'}
			<span class="hc-quiet note"
				>Tap to run. The bridge doesn't say which scene is “on”, so none is shown lit.</span
			>
		{/if}
	{:else}
		<p class="hc-quiet">
			No scenes yet — add the bridge's scenes in the plugin's settings (the integration report has
			them).
		</p>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}
</div>

<style>
	.card {
		height: 100%;
		gap: 10px;
	}
	.top {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 10px;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-size: 13px;
		white-space: nowrap;
	}
	.scenes {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 8px;
	}
	.scenes.row {
		display: flex;
		overflow-x: auto;
		scrollbar-width: thin;
	}
	.scene {
		min-height: 48px;
		padding: 0 12px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: color-mix(in srgb, var(--bg, #f3f0e8) 50%, var(--surface));
		color: var(--ink);
		padding: 6px 10px;
		font-size: 14px;
		font-weight: 600;
		line-height: 1.25;
		overflow-wrap: anywhere;
		min-width: 0;
	}
	.row .scene {
		flex: none;
		white-space: nowrap;
		min-height: 44px;
	}
	.scene.busy {
		opacity: 0.6;
	}
	.scene:disabled {
		cursor: default;
	}
	.note {
		font-size: 12px;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 13px;
	}
	p {
		margin: 0;
	}
</style>
