<svelte:options
	customElement={{
		tag: 'hc-zwave-lock-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A lock's card on Home and the rooms: locked or not (and the door), how it
     last changed, its battery, and Lock / Unlock — unlocking asks first.
     Compact: one row. -->
<script>
	import { adopt, cardLine, compactLine, slotNames } from './lib/zwave.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);
	const locked = $derived(a.locked === true);
	const state = $derived(a.jammed === true ? 'Jammed' : locked ? 'Locked' : a.locked === false ? 'Unlocked' : 'Not sure');

	let names = $state({});
	$effect(() => {
		if (hc && deviceId) void slotNames(hc, deviceId).then((r) => (names = r.names));
	});

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

{#if dev}
	<div class="hc-tile {form}" class:off={!dev.available}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		{#if form === 'compact'}
			<span class="row">
				{@render icon(18)}
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="hc-tile-state one">{dev.available === false ? 'Offline' : compactLine(dev)}</span>
				</span>
				<button class="act ghost" disabled={!canRun || busy} onclick={toggle}>{locked ? 'Unlock' : 'Lock'}</button>
			</span>
		{:else}
			<span class="row">
				{@render icon(22)}
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="st" class:open={!locked}>{dev.available === false ? 'Offline' : `${state}${a.open === true ? ' · door open' : ''}`}</span>
				</span>
			</span>
			{#if cardLine(dev, names)}<span class="hc-tile-state line">{cardLine(dev, names)}</span>{/if}
			<button class="act" disabled={!canRun || busy} onclick={toggle}>{locked ? 'Unlock' : 'Lock'}</button>
			{#if error}<span class="err" role="alert">{error}</span>{/if}
		{/if}
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
	.compact .row {
		gap: 10px;
	}
	.row .act {
		pointer-events: auto;
	}
	.line {
		pointer-events: none;
	}
	.off {
		opacity: 0.7;
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
	.compact .ic {
		border-radius: 10px;
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
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
		font-weight: 600;
	}
	.act.ghost {
		margin-top: 0;
		flex: none;
		min-height: 40px;
		padding: 0 12px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font-weight: 400;
	}
	.act:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.err {
		font-size: 12px;
		color: var(--alert);
	}
	.standard .hc-tile-name,
	.large .hc-tile-name {
		white-space: normal;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		-webkit-box-orient: vertical;
	}
	.line {
		font-size: 13px;
	}
	@container (max-width: 240px) {
		.standard .row,
		.large .row {
			gap: 10px;
		}
		.standard .ic,
		.large .ic {
			width: 36px !important;
			height: 36px !important;
		}
		.st {
			font-size: 13px;
		}
	}
</style>
