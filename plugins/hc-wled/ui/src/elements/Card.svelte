<svelte:options
	customElement={{
		tag: 'hc-wled-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A WLED controller's card: its look as a strip of its colours, what it
     runs, on or off and how bright — and, large, its presets to tap. Tap
     the name for its panel. -->
<script lang="ts">
	import type { Host } from '#lib/host.js';
	import { devices } from '#lib/devices.svelte.js';
	import { commands } from '#lib/commands.svelte.js';
	import { wledCatalogs } from '#lib/wledCatalog.svelte.js';
	import { effectNames, mainSegment, nameOf, presetCss, presetsOf, segmentCss } from '#lib/wled.js';
	import { start } from './adopt.js';

	let {
		host: hc,
		deviceId,
		form = 'standard'
	}: { host: Host; deviceId?: string; form?: 'compact' | 'standard' | 'large' } = $props();
	const me = $host();
	start(me, hc);
	$effect(() => start(me, hc));

	const d = $derived(deviceId ? devices.byId[deviceId] : undefined);
	$effect(() => {
		if (d && form !== 'compact') wledCatalogs.ensure(d);
	});
	const a = $derived(d?.attributes ?? {});
	const on = $derived(a.on === true);
	const seg = $derived(d ? mainSegment(d) : undefined);
	const presets = $derived(d ? presetsOf(d).filter((p) => p.kind !== 'playlist') : []);
	const running = $derived(d ? presetsOf(d).find((p) => p.id === a.preset_id) : undefined);
	const fx = $derived(d ? nameOf(effectNames(d), seg?.effect_id) : undefined);
	const said = $derived(
		!d?.available ? 'Offline' : !on ? 'Off' : running ? running.name : fx ? fx : 'On'
	);
	const pct = $derived(typeof a.brightness_pct === 'number' ? Math.round(a.brightness_pct) : 0);
	const may = $derived(!!d?.available && (hc?.me.can('devices:command') || hc?.me.can('devices:write')));
	const toggle = () => d && commands.send(d.device_id, { on: !on });
	const open = () => d && hc.ui.openDevice(d.device_id);
</script>

{#if d}
	<div class="hc-tile {form}" class:lit={on && d.available}>
		<div class="top">
			<button class="name" onclick={open}>
				<span class="hc-tile-name">{d.name_override ?? d.name}</span>
				<span class="hc-tile-state">{said}{on && form !== 'compact' ? ` · ${pct}%` : ''}</span>
			</button>
			<button
				class="power"
				role="switch"
				aria-checked={on}
				aria-label={`${d.name_override ?? d.name} ${on ? 'off' : 'on'}`}
				disabled={!may}
				onclick={toggle}><span></span></button
			>
		</div>
		{#if form !== 'compact'}
			<div
				class="strip"
				class:dim={!on}
				style:background={segmentCss(seg, wledCatalogs.of(d).catalog)}
				aria-hidden="true"
			></div>
			<input
				type="range"
				min="1"
				max="100"
				value={pct || 50}
				disabled={!may}
				aria-label="Brightness"
				onchange={(e) =>
					commands.send(d.device_id, { brightness_pct: Number(e.currentTarget.value) })}
			/>
		{/if}
		{#if form === 'large' && presets.length}
			<div class="presets">
				{#each presets.slice(0, 12) as p (p.id)}
					<button
						class="preset"
						class:on={on && p.id === a.preset_id}
						disabled={!may}
						onclick={() => commands.act(d.device_id, 'apply_preset', { preset_id: p.id })}
					>
						<span class="sw" style:background={presetCss(p)}></span>{p.name}
					</button>
				{/each}
			</div>
		{/if}
	</div>
{/if}

<style>
	.top {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 10px;
	}
	.compact .top {
		align-items: center;
	}
	.name {
		display: flex;
		flex-direction: column;
		min-width: 0;
		padding: 0;
		border: none;
		background: none;
		text-align: left;
	}
	.power {
		flex: none;
		width: 46px;
		height: 28px;
		border-radius: 14px;
		border: none;
		background: var(--track, var(--line));
		position: relative;
	}
	.power span {
		position: absolute;
		top: 3px;
		left: 3px;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: var(--sheet, #fff);
		transition: transform 0.15s;
	}
	.power[aria-checked='true'] {
		background: var(--accent);
	}
	.power[aria-checked='true'] span {
		transform: translateX(18px);
	}
	.power:disabled {
		opacity: 0.5;
	}
	.strip {
		height: 10px;
		border-radius: 5px;
	}
	.strip.dim {
		opacity: 0.35;
	}
	input[type='range'] {
		width: 100%;
		accent-color: var(--track-on, var(--accent));
	}
	.presets {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.preset {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-height: 32px;
		padding: 0 10px 0 6px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 13px;
	}
	.preset.on {
		border-color: var(--accent);
		font-weight: 600;
	}
	.sw {
		width: 18px;
		height: 18px;
		border-radius: 5px;
	}
</style>
