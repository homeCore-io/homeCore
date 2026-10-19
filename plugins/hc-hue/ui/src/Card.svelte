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

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

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
		if (!dev.available) return 'Not answering';
		if (kind === 'motion_sensor') return a.motion ? 'Motion now' : 'No motion';
		if (kind === 'group')
			return onCount === 0 ? 'All off' : `${onCount} of ${members.length} on${running ? ` · ${hc.devices.label(running)}` : ''}`;
		if (!a.on) return 'Off';
		return `${bri} %${form === 'large' && running ? ` · ${hc.devices.label(running)}` : ''}`;
	});

	const open = () => dev && hc.ui.openDevice(dev.device_id);
	const toggle = () => hc.command(deviceId, { on: !a.on });
</script>

{#if dev}
	<div class="hc-tile {form}" class:lit class:off={!dev.available}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		{#if kind === 'motion_sensor'}
			<span class="row">
				<span class="hc-tile-name">{name}</span>
				{#if num(a.battery_pct) !== undefined && num(a.battery_pct) <= 15}<span class="bat">battery {Math.round(num(a.battery_pct))} %</span>{/if}
			</span>
			<span class="hc-tile-state">{stateLine}</span>
			{#if form !== 'compact'}
				<span class="row facts">
					{#if num(a.temperature) !== undefined}<span class="hc-num">{num(a.temperature).toFixed(1)}°</span>{/if}
					{#if num(a.illuminance) !== undefined}<span class="hc-num">{Math.round(num(a.illuminance))} lux</span>{/if}
				</span>
			{/if}
		{:else}
			<span class="row">
				{#if form === 'compact' || form === 'large'}
					<span class="sw" class:big={form === 'large'} style:background={kind === 'group' ? 'none' : swatchOf(a)}></span>
				{/if}
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="hc-tile-state">{stateLine}</span>
				</span>
				{#if dev.available}
					<button class="tog" class:off={!a.on} role="switch" aria-checked={!!a.on} aria-label={`${name} on or off`} onclick={toggle}
					></button>
				{/if}
			</span>
			{#if form !== 'compact'}
				{#if kind === 'group'}
					<span class="stripe">
						{#each members as l (l.device_id)}<span style:background={swatchOf(l.attributes)} title={hc.devices.label(l)}></span>{/each}
					</span>
				{:else}
					<span class="bar"><span style:width={`${a.on ? bri : 0}%`} style:background={swatchOf(a)}></span></span>
				{/if}
				{#if form === 'large' && scenes.length}
					<span class="scenes"><Scenes {hc} {scenes} columns={4} compact /></span>
				{/if}
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
	.off {
		opacity: 0.6;
	}
	.sw {
		width: 30px;
		height: 30px;
		border-radius: 9px;
		flex: none;
	}
	.sw.big {
		width: 40px;
		height: 40px;
		border-radius: 11px;
	}
	.lit .sw {
		box-shadow: 0 0 14px rgba(255, 170, 80, 0.45);
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
	}
	.tog.off {
		background: var(--track-off, #cfc8ba);
	}
	.tog.off::after {
		right: auto;
		left: 3px;
	}
</style>
