<svelte:options
	customElement={{
		tag: 'hc-yolink-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		},
		extend: (C) =>
			class extends C {
				/** Locks and temperature probes (a freezer) get this card; a
				 * room's temperature sensor keeps the app's own. */
				static accepts(device) {
					return isLock(device) || isProbe(device);
				}
			}
	}}
/>

<!-- A YoLink lock's card — locked or not (and the door), how it last
     changed, a low battery, Lock / Unlock (unlocking asks first) — or a
     temperature probe's: the reading, and what homeCore tells you about it. -->
<script module>
	import { isLock, isProbe } from './lib/yolink.js';
</script>

<script>
	import { adopt, batteryLow, cardLine, lockState, temp } from './lib/yolink.js';
	import { cardWords, readRules, rulesFor } from './lib/tell.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const lock = $derived(isLock(dev));
	const off = $derived(dev?.available === false);
	const canRun = $derived(!!hc?.me.can('devices:command') && !off);
	const locked = $derived(a.locked === true);
	let unitsTick = $state(0);
	$effect(() => hc?.units?.subscribe?.(() => unitsTick++));

	// A probe's rules, where this person may read them.
	let rules = $state([]);
	const probe = $derived(!!dev && !lock);
	$effect(() => {
		if (hc && probe) void readRules(hc).then((r) => (rules = r.rules));
	});
	const tells = $derived((void unitsTick, dev && !lock ? cardWords(hc, rulesFor(rules, dev.device_id)) : ''));

	let error = $state('');
	let busy = $state(false);
	async function toggle() {
		error = '';
		if (locked) {
			const ok = await hc.ui.confirm({ title: `Unlock ${name}?`, body: 'The door opens to anyone.', go: 'Unlock' });
			if (!ok) return;
		}
		busy = true;
		const r = await hc.command(deviceId, { locked: !locked });
		busy = false;
		if (!r.ok) error = r.error;
	}
	const open = () => dev && hc.ui.openDevice(dev.device_id);
	const line = $derived([cardLine(dev), batteryLow(dev) ? 'battery low' : ''].filter(Boolean).join(' · '));
</script>

{#snippet icon(size)}
	<span class="ic" class:shut={locked} style:width={`${size + 22}px`} style:height={`${size + 22}px`}>
		<svg width={size} height={size} viewBox="0 0 22 22" aria-hidden="true">
			<rect x="4" y="10" width="14" height="9" rx="2" fill="none" stroke="currentColor" stroke-width="1.7"></rect>
			{#if locked}
				<path d="M7.5 10V7a3.5 3.5 0 0 1 7 0v3" fill="none" stroke="currentColor" stroke-width="1.7"></path>
			{:else}
				<path d="M7.5 10V7a3.5 3.5 0 0 1 6.8-1.2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"></path>
			{/if}
		</svg>
	</span>
{/snippet}

{#if dev && lock}
	<div class="hc-tile {form}" class:off>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		{#if form === 'compact'}
			<span class="row">
				{@render icon(18)}
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="hc-tile-state one">{off ? 'Not reporting' : [lockState(dev), cardLine(dev)].filter(Boolean).join(' · ')}</span>
				</span>
				<button class="act ghost" disabled={!canRun || busy} onclick={toggle}>{locked ? 'Unlock…' : 'Lock'}</button>
			</span>
		{:else}
			<span class="row">
				{@render icon(22)}
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="st" class:open={!locked}>{off ? 'Not reporting' : `${lockState(dev)}${a.open === true ? ' · door open' : ''}`}</span>
				</span>
			</span>
			{#if line}<span class="hc-tile-state line" class:low={batteryLow(dev)}>{line}</span>{/if}
			<button class="act" disabled={!canRun || busy} onclick={toggle}>{locked ? 'Unlock…' : 'Lock'}</button>
			{#if error}<span class="err" role="alert">{error}</span>{/if}
		{/if}
	</div>
{:else if dev}
	<div class="hc-tile {form} probe" class:off>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		<span class="hc-tile-name">{name.replace(/ (temperature|temp sensor|temperature sensor)$/i, '')}</span>
		<span class="reading hc-num" class:bad={off || a.too_warm === true || a.too_cold === true}>{off ? 'Not reporting' : (void unitsTick, temp(hc, a.temperature))}</span>
		{#if tells && form !== 'compact'}<span class="hc-tile-state">{tells}</span>{/if}
	</div>
{/if}

<style>
	.hc-tile {
		position: relative;
		height: 100%;
		gap: 12px;
		padding: 16px;
		container-type: inline-size;
	}
	.hc-tile.compact {
		padding: 10px 12px;
	}
	.hc-tile.probe {
		gap: 6px;
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
	.line,
	.act,
	.err {
		position: relative;
		z-index: 1;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
		pointer-events: none;
	}
	.row .act {
		pointer-events: auto;
	}
	.line {
		pointer-events: none;
		font-size: 13px;
	}
	.line.low {
		color: var(--alert);
	}
	.off {
		opacity: 0.75;
	}
	.ic {
		flex: none;
		display: flex;
		align-items: center;
		justify-content: center;
		border-radius: 12px;
		color: var(--alert);
		background: color-mix(in srgb, var(--alert-bg, #fbe9e2) 80%, var(--surface));
	}
	.ic.shut {
		color: light-dark(#2f5a27, #8fd19e);
		background: light-dark(#e6f0e1, #1f3a26);
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.st {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.st.open {
		color: var(--alert);
	}
	.one {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.act {
		margin-top: auto;
		min-height: 44px;
		border-radius: 11px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.act.ghost {
		margin-top: 0;
		flex: none;
		min-height: 40px;
		padding: 0 12px;
		border-radius: 10px;
	}
	.act:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.err {
		font-size: 12px;
		color: var(--alert);
	}
	.reading {
		font-size: 30px;
		font-weight: 600;
		line-height: 1.15;
		pointer-events: none;
	}
	.compact .reading {
		font-size: 20px;
	}
	.reading.bad {
		color: var(--alert);
	}
	.probe .hc-tile-name,
	.probe .hc-tile-state {
		pointer-events: none;
	}
</style>
