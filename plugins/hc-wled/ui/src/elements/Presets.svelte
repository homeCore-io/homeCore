<svelte:options
	customElement={{
		tag: 'hc-wled-presets',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Quick presets", for a page: the presets chosen in its settings, each a
     button that applies it on the controllers chosen there — by name, on
     each, since the same preset may sit in another slot on each. -->
<script lang="ts">
	import type { Host } from '#lib/host.js';
	import { devices } from '#lib/devices.svelte.js';
	import { commands } from '#lib/commands.svelte.js';
	import { presetCss, presetOn, presetsOf } from '#lib/wled.js';
	import { start } from './adopt.js';

	type Config = { device_ids?: string[]; presets?: number[]; title?: string };
	let {
		host: hc,
		form = 'standard',
		config = {}
	}: { host: Host; form?: string; config?: Config } = $props();
	const me = $host();
	start(me, hc);
	$effect(() => start(me, hc));

	const targets = $derived((config.device_ids ?? []).map((id) => devices.byId[id]).filter((d) => !!d));
	const first = $derived(targets[0]);
	const chosen = $derived(
		first ? presetsOf(first).filter((p) => (config.presets ?? []).includes(p.id)) : []
	);
	const running = (id: number) =>
		!!first && first.attributes.on === true && first.attributes.preset_id === id;
	const may = $derived(hc?.me.can('devices:command') || hc?.me.can('devices:write'));
	function apply(id: number) {
		if (!first) return;
		for (const d of targets) {
			const p = d === first ? id : presetOn(d, first, id);
			if (p !== undefined) void commands.act(d.device_id, 'apply_preset', { preset_id: p });
		}
	}
</script>

<div class="hc-tile">
	<span class="hc-tile-name">{config.title || 'Lights'}</span>
	{#if !first || chosen.length === 0}
		<p class="hc-tile-state">Choose the controllers and presets in this card’s settings.</p>
	{:else}
		<span class="hc-tile-state"
			>{targets.map((d) => d.name_override ?? d.name).join(', ')}</span
		>
		<div class="presets" class:large={form === 'large'}>
			{#each chosen as p (p.id)}
				<button class="preset" class:on={running(p.id)} disabled={!may} onclick={() => apply(p.id)}>
					<span class="sw" style:background={presetCss(p)}></span>
					<span class="label">{p.name}</span>
				</button>
			{/each}
			<button
				class="preset off"
				disabled={!may}
				onclick={() => targets.forEach((d) => commands.send(d.device_id, { on: false }))}>Off</button
			>
		</div>
	{/if}
</div>

<style>
	.presets {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(110px, 1fr));
		gap: 6px;
	}
	.preset {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 6px;
		min-height: 52px;
		padding: 6px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 13px;
		text-align: left;
	}
	.preset.on {
		border-color: var(--accent);
		background: var(--lit-bg);
		font-weight: 600;
	}
	.preset.off {
		justify-content: center;
		text-align: center;
		color: var(--ink-muted);
	}
	.sw {
		height: 14px;
		border-radius: 4px;
	}
	/* One column wide: slim rows, a swatch beside each name. */
	.presets:not(.large) {
		grid-template-columns: 1fr;
		gap: 4px;
	}
	.presets:not(.large) .preset {
		flex-direction: row;
		align-items: center;
		min-height: 34px;
		padding: 4px 8px 4px 4px;
	}
	.presets:not(.large) .sw {
		flex: none;
		width: 22px;
		height: 22px;
		border-radius: 6px;
	}
	.label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
