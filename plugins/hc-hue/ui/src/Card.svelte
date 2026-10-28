<svelte:options
	customElement={{
		tag: 'hc-hue-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A Hue device's card on Home and the rooms. A light glows in its own
     colour; a room shows each of its lights; large, both carry the room's
     scenes to tap. A motion sensor says when it last saw someone. -->
<script>
	import Scenes from './lib/SceneTiles.svelte';
	import { adopt, ago, num, runningIn, scenesOf, swatchOf } from './lib/hue.js';
	import { follow, temperature } from './lib/units.svelte.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));
	$effect(() => follow(hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const kind = $derived(dev?.device_type);
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const roomRid = $derived(kind === 'group' ? a.group_rid : a.room_rid);
	const scenes = $derived(scenesOf(devices, roomRid).slice(0, 4));
	const running = $derived(runningIn(devices, roomRid));
	const members = $derived(
		kind === 'group'
			? (Array.isArray(a.light_ids) ? a.light_ids : []).map((id) => devices.find((d) => d.device_id === id)).filter(Boolean)
			: []
	);
	const onCount = $derived(members.filter((l) => l.attributes.on).length);
	const lit = $derived(kind === 'motion_sensor' ? a.motion === true : !!a.on);
	const bri = $derived(Math.round(num(a.brightness_pct) ?? 0));

	const stateLine = $derived.by(() => {
		if (!dev) return '';
		if (!dev.available) return 'Offline';
		if (kind === 'motion_sensor') return a.motion ? 'Motion now' : 'No motion';
		if (kind === 'group')
			return onCount === 0 ? 'All off' : `${onCount} of ${members.length} on${running ? ` · ${hc.devices.label(running)}` : ''}`;
		if (!a.on) return 'Off';
		return `${bri}%${form === 'large' && running ? ` · ${hc.devices.label(running)}` : ''}`;
	});

	const open = () => dev && hc.ui.openDevice(dev.device_id);
	const toggle = () => hc.command(deviceId, { on: !a.on });
</script>

{#snippet icon()}
	<!-- Drawn as the app draws its own (Lucide): a bulb, a room's lamp, motion. -->
	<span class="ic" aria-hidden="true"
		><svg viewBox="0 0 24 24" width={form === 'compact' ? 18 : 20} height={form === 'compact' ? 18 : 20} fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
			>{#if kind === 'motion_sensor'}<path d="M22 12h-4l-3 9L9 3l-3 9H2" />{:else if kind === 'group'}<path d="M8 2h8l4 10H4L8 2Z" /><path d="M12 12v6" /><path d="M8 22v-2c0-1.1.9-2 2-2h4a2 2 0 0 1 2 2v2H8Z" />{:else}<path
					d="M15 14c.2-1 .7-1.7 1.5-2.5 1-.9 1.5-2.2 1.5-3.5A6 6 0 0 0 6 8c0 1 .2 2.2 1.5 3.5.7.7 1.3 1.5 1.5 2.5"
				/><path d="M9 18h6" /><path d="M10 22h4" />{/if}</svg
		></span
	>
{/snippet}

{#snippet words()}
	<span class="words">
		<span class="hc-tile-name">{name}</span>
		<span class="hc-tile-state">{stateLine}{#if kind === 'motion_sensor' && num(a.battery_pct) !== undefined && num(a.battery_pct) <= 15}<span class="bat"> · battery {Math.round(num(a.battery_pct))}%</span>{/if}</span>
	</span>
{/snippet}

{#if dev}
	<!-- The app's own card, laid out the same (#5): its symbol top left, the
	     switch top right, the name and state under them; the light's colour
	     in the bar, a room's lights in its stripe. -->
	<div class="hc-tile {form}" class:lit class:gone={!dev.available}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		<span class="top">
			{@render icon()}
			{#if form === 'compact'}{@render words()}{/if}
			{#if dev.available && kind !== 'motion_sensor'}
				<button class="tog" class:off={!a.on} role="switch" aria-checked={!!a.on} aria-label={`${name} on or off`} onclick={toggle}
				></button>
			{/if}
		</span>
		{#if form !== 'compact'}
			{@render words()}
			{#if kind === 'motion_sensor'}
				<span class="row facts">
					{#if num(a.temperature) !== undefined}<span class="hc-num">{temperature(num(a.temperature)).n}°</span>{/if}
					{#if num(a.illuminance) !== undefined}<span class="hc-num">{Math.round(num(a.illuminance))} lux</span>{/if}
				</span>
			{:else if kind === 'group'}
				<span class="stripe">
					{#each members as l (l.device_id)}<span style:background={swatchOf(l.attributes)} title={hc.devices.label(l)}></span>{/each}
				</span>
			{:else}
				<span class="bar"><span style:width={`${a.on ? bri : 0}%`} style:background={swatchOf(a)}></span></span>
			{/if}
			{#if form === 'large' && scenes.length && kind !== 'motion_sensor'}
				<span class="scenes"><Scenes {hc} {scenes} columns={4} compact /></span>
			{/if}
		{/if}
	</div>
{/if}

<style>
	.hc-tile {
		position: relative;
		height: 100%;
	}
	.cover {
		position: absolute;
		inset: 0;
		border: none;
		background: none;
		border-radius: inherit;
		z-index: 0;
	}
	.row,
	.words,
	.stripe,
	.bar,
	.scenes,
	.tog,
	.hc-tile-state,
	.hc-tile-name {
		position: relative;
		z-index: 1;
		pointer-events: none;
	}
	.tog,
	.scenes {
		pointer-events: auto;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
	}
	.words {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
	}
	/* Offline: dimmed, as the app's own (not .off — that's a light switched off). */
	.gone .ic,
	.gone .words {
		opacity: 0.55;
	}
	.top {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 10px;
		min-width: 0;
		position: relative;
		z-index: 1;
		pointer-events: none;
	}
	.compact .top {
		align-items: center;
	}
	.compact .words {
		flex: 1;
	}
	.ic {
		width: 38px;
		height: 38px;
		flex: none;
		border-radius: var(--radius-chip, 10px);
		background: var(--surface-2);
		color: var(--ink-muted);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.compact .ic {
		width: 34px;
		height: 34px;
	}
	.lit .ic {
		background: var(--fill, var(--lit-bg));
		color: var(--lit-ink);
	}
	.bar {
		height: 9px;
		border-radius: 5px;
		background: color-mix(in srgb, var(--ink) 10%, transparent);
		overflow: hidden;
		margin-top: auto;
	}
	.bar span {
		display: block;
		height: 100%;
		border-radius: 5px;
	}
	.stripe {
		display: flex;
		gap: 4px;
		margin-top: auto;
	}
	.stripe span {
		flex: 1;
		height: 18px;
		border-radius: 6px;
	}
	.facts {
		margin-top: auto;
		gap: 16px;
		font-size: 14px;
	}
	.bat {
		font-size: 12px;
		color: var(--alert);
	}
	.tog {
		width: 46px;
		height: 28px;
		border-radius: 14px;
		background: var(--track-on, var(--accent));
		flex: none;
		border: none;
		transition: background 0.15s;
	}
	.tog::after {
		content: '';
		position: absolute;
		right: 3px;
		top: 3px;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: #fff;
		/* As the app's own switch, so it reads the same in dark mode (#123). */
		box-shadow: 0 1px 3px rgba(0, 0, 0, 0.3);
	}
	.tog.off {
		background: var(--track-off, #cfc8ba);
	}
	.tog.off::after {
		right: auto;
		left: 3px;
	}
</style>
