<svelte:options
	customElement={{
		tag: 'hc-lutron-scenes',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Lutron scenes", a card you add: the scenes you chose, in your order,
     lit as the repeater lights them; a tap runs one. A grid, or one row. -->
<script>
	import { adopt, reportsState, sceneGroups, sceneOn } from './lib/lutron.js';

	let { host: hc, form = 'standard', config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const all = $derived(sceneGroups(devices).flatMap((g) => g.scenes));
	const chosen = $derived(
		Array.isArray(config?.scenes) && config.scenes.length
			? config.scenes.map((id) => all.find((s) => s.device_id === id)).filter(Boolean)
			: all.slice(0, 6)
	);
	const title = $derived(config?.name || 'Scenes');
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
		{#if form !== 'compact' && all.length}<button class="link" onclick={page}>All {all.length} ›</button>{/if}
	</div>
	{#if chosen.length}
		<div class="scenes" class:row={form === 'compact'}>
			{#each chosen as s (s.device_id)}
				<button
					class="scene"
					class:on={sceneOn(s)}
					class:busy={busy === s.device_id}
					aria-pressed={reportsState(s) ? sceneOn(s) : undefined}
					disabled={!canRun || s.available === false}
					onclick={() => run(s)}
				>
					<span class="led" class:on={sceneOn(s)} class:none={!reportsState(s)}></span><span class="name">{hc.devices.label(s)}</span>
				</button>
			{/each}
		</div>
	{:else}
		<p class="hc-quiet">No scenes yet — the plugin publishes the repeater's phantom buttons once it has read its layout.</p>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}
</div>

<style>
	.card {
		height: 100%;
		gap: 12px;
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
		grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
		gap: 8px;
	}
	.scenes.row {
		display: flex;
		overflow-x: auto;
		scrollbar-width: thin;
	}
	.scene {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 52px;
		padding: 0 12px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--sheet, var(--surface));
		text-align: left;
		min-width: 0;
	}
	.row .scene {
		flex: none;
		min-height: 44px;
	}
	.scene.on {
		background: color-mix(in srgb, var(--track-on) 9%, var(--surface));
		border-color: color-mix(in srgb, var(--track-on) 55%, var(--surface));
	}
	.scene.busy {
		opacity: 0.6;
	}
	.name {
		font-size: 14px;
		font-weight: 600;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.led {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--track-off);
	}
	.led.on {
		background: var(--track-on);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--track-on) 30%, transparent);
	}
	.led.none {
		background: none;
		border: 1.5px dashed var(--track-off);
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 13px;
	}
</style>
