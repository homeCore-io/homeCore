<svelte:options customElement={{ tag: 'hc-zwave-page', props: { host: { type: 'Object' } } }} />

<!-- Z-Wave's plugin page: whether Z-Wave JS answers and how many devices do;
     what needs someone (a low battery, a device not answering, one no longer
     in the network); every device with its room, what it is doing, its power,
     its signal and when it was last heard; the controller; rebuilding routes
     and backing up the stick. Adding and removing devices open sheets. -->
<script>
	import AddDevice from './lib/AddDevice.svelte';
	import RemoveDevice from './lib/RemoveDevice.svelte';
	import {
		adopt,
		ago,
		attention,
		downloadBackup,
		failed,
		follow,
		gone,
		listWords,
		lowBattery,
		modelLine,
		nowWords,
		powerWords,
		rowsOf,
		settingsHref,
		SIGNAL_WORDS,
		start,
		cancel
	} from './lib/zwave.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));

	let status = $state(null);
	let statusError = $state('');
	let tick = $state(Date.now());
	async function readStatus() {
		const r = await hc.query('status');
		if (r.ok && r.data?.status !== 'error') {
			status = r.data;
			statusError = '';
		} else statusError = failed(r);
	}
	$effect(() => {
		if (!hc) return;
		void readStatus();
		const t = setInterval(() => {
			tick = Date.now();
			void readStatus();
		}, 10000);
		return () => clearInterval(t);
	});

	// A device added or removed: ask again at once, so the list and the
	// network agree.
	const idsKey = $derived(devices.map((d) => d.device_id).sort().join(','));
	$effect(() => {
		if (hc && idsKey) void readStatus();
	});
	const connected = $derived(status?.connection?.state === 'connected');
	const rows = $derived(
		rowsOf(status, devices).sort((a, b) => (a.device ? label(a.device) : a.node.name ?? '').localeCompare(b.device ? label(b.device) : b.node.name ?? ''))
	);
	const answering = $derived(rows.filter((r) => r.node.status !== 'dead').length);
	const c = $derived(status?.controller ?? {});
	const lows = $derived(rows.filter((r) => lowBattery(r.node, r.device)));
	const dead = $derived(rows.filter((r) => r.node.status === 'dead'));
	const leftover = $derived(gone(status, devices));
	const nameOf = (r) => (r.device ? label(r.device) : r.node.name || `Node ${r.node.node_id}`);

	let error = $state('');
	let adding = $state(null); // null | { replace, name }
	let removing = $state(false);

	// Rebuilding routes, with each device's progress.
	let rebuild = $state(null); // { rid, percent, message, done }
	async function rebuildRoutes() {
		error = '';
		const r = await start(hc, 'rebuild_routes');
		if (r.error) {
			error = r.error;
			return;
		}
		rebuild = { rid: r.rid, percent: 0, message: 'Starting' };
		follow(hc, r.rid, (e) => {
			if (e.stage === 'progress') rebuild = { ...rebuild, percent: e.percent ?? rebuild.percent, message: e.message ?? rebuild.message };
			else if (e.stage === 'complete') {
				const d = e.data ?? {};
				rebuild = { done: true, message: `Done: ${d.done ?? 0} rebuilt${d.skipped ? `, ${d.skipped} asleep (they update when they wake)` : ''}${d.failed ? `, ${d.failed} didn’t answer` : ''}.` };
				void readStatus();
			} else if (e.stage === 'error' || e.stage === 'timeout') rebuild = { done: true, message: e.message ?? 'It stopped' };
			else if (e.stage === 'canceled') rebuild = null;
		});
	}
	const stopRebuild = () => rebuild?.rid && cancel(hc, rebuild.rid);

	// The controller backup.
	let backingUp = $state('');
	let backedUp = $state('');
	async function backup() {
		error = '';
		backedUp = '';
		backingUp = 'Reading the stick…';
		const err = await downloadBackup(hc, (i, n) => (backingUp = `Downloading… ${Math.round((i / n) * 100)}%`));
		backingUp = '';
		if (err) error = err;
		else backedUp = `Saved — keep it somewhere safe.`;
	}

	async function forget(d) {
		const ok = await hc.ui.confirm({
			title: `Remove ${label(d)} from homeCore?`,
			body: 'It is no longer in the Z-Wave network, so it can’t come back by itself.',
			lost: ['Its room', 'Its place in automations and on dashboards'],
			go: 'Remove',
			danger: true
		});
		if (!ok) return;
		const r = await hc.action('forget_device', { device_id: d.device_id });
		error = failed(r);
		void readStatus();
	}

	const sigTone = (s) => (s === 'good' ? 'good' : s === 'fair' ? 'warn' : s === 'poor' ? 'bad' : '');
	const heard = (n) => (n.last_seen ? ago(n.last_seen, tick) : n.status === 'asleep' ? 'asleep' : '—');
</script>

<div class="page">
	<header class="top">
		<div class="status">
			<span class="pill {!connected ? 'bad' : dead.length ? 'warn' : 'good'}"
				><span class="hc-dot {!connected ? 'bad' : dead.length ? 'warn' : 'good'}"></span>{!status
					? 'Asking Z-Wave JS…'
					: !connected
						? 'Z-Wave JS is not answering'
						: dead.length
							? `${answering} of ${rows.length} answering`
							: `${rows.length} device${rows.length === 1 ? '' : 's'} · all answering`}</span
			>
			{#if connected}
				<span class="hc-quiet">Connected to Z-Wave JS{c.model ? ` · controller ${c.model}${c.location ? ` in ${c.location}` : ''}` : ''}</span>
			{:else if status?.connection?.last_error}
				<span class="hc-quiet">{status.connection.last_error}</span>
			{/if}
		</div>
		<span class="hc-row">
			{#if admin}
				<button class="hc-pill go big" disabled={!connected} onclick={() => (adding = { replace: null })}>Add a device</button>
				<button class="hc-pill big" disabled={!connected} onclick={() => (removing = true)}>Remove a device</button>
				<button class="hc-pill big" onclick={() => hc.ui.navigate(settingsHref(hc))}>Settings</button>
			{/if}
		</span>
	</header>

	{#if statusError}<p class="err" role="alert">{statusError}</p>{/if}
	{#if status && !connected}
		<div class="note bad">
			<div class="words">
				<strong>Every Z-Wave device is offline until Z-Wave JS answers</strong>
				<span class="hc-quiet">Commands are not kept for later: anything sent while it is away fails and says so. This page fills in again when it is back.</span>
			</div>
		</div>
	{/if}
	{#each lows as r (r.node.device_id)}
		<div class="note">
			<div class="words">
				<strong>{nameOf(r)}’s battery is at {r.node.battery_pct}%</strong>
				<span class="hc-quiet">{r.device?.device_type === 'lock' ? 'Locks stop answering well below 20%. Change the batteries soon.' : 'Change the battery soon.'}</span>
			</div>
			<button class="link" onclick={() => hc.ui.openDevice(r.node.device_id)}>Open {r.device?.device_type === 'lock' ? 'the lock' : 'it'} ›</button>
		</div>
	{/each}
	{#if dead.length}
		<div class="note bad">
			<div class="words">
				<strong>{listWords(dead.map(nameOf))} {dead.length === 1 ? 'isn’t' : 'aren’t'} answering</strong>
				<span class="hc-quiet">Shown offline, kept with {dead.length === 1 ? 'its' : 'their'} room and automations. Open {dead.length === 1 ? 'it' : 'one'} to ping it, or — if it’s gone for good — remove or replace it.</span>
			</div>
			<span class="hc-row">{#each dead as r (r.node.device_id)}<button class="hc-pill" onclick={() => hc.ui.openDevice(r.node.device_id)}>Open {nameOf(r)}</button>{/each}</span>
		</div>
	{/if}
	{#if leftover.length}
		<div class="note">
			<div class="words">
				<strong>{listWords(leftover.map(label))} {leftover.length === 1 ? 'is' : 'are'} no longer in the Z-Wave network</strong>
				<span class="hc-quiet">Removed with another app, or reset. homeCore keeps {leftover.length === 1 ? 'it' : 'them'} until you say.</span>
			</div>
			{#if admin}
				<span class="hc-row">{#each leftover as d (d.device_id)}<button class="hc-pill danger" onclick={() => forget(d)}>Remove {label(d)}</button>{/each}</span>
			{/if}
		</div>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	<section class="sect">
		<div class="head">
			<h2>Devices</h2>
			<span class="hc-quiet">Tap one for its health and settings</span>
		</div>
		<div class="table">
			<div class="tr th">
				<span>Device</span><span>Room</span><span>Now</span><span>Power</span><span>Signal</span><span>Last heard</span>
			</div>
			{#each rows as r (r.node.device_id)}
				<button class="tr" class:dead={r.node.status === 'dead'} onclick={() => hc.ui.openDevice(r.node.device_id)}>
					<span class="dev"><b>{nameOf(r)}</b><small>{modelLine(r.node, r.device)}</small></span>
					<span class="room">{r.device?.room ?? ''}</span>
					<span class="now">{r.node.status === 'dead' ? 'Not answering' : nowWords(r.device)}</span>
					<span class="pow" class:low={lowBattery(r.node, r.device)}>{powerWords(r.node)}</span>
					<span class="sig"><span class="hc-dot {sigTone(r.node.signal)}"></span>{SIGNAL_WORDS[r.node.signal] ?? '—'}</span>
					<span class="heard hc-num">{heard(r.node)}</span>
				</button>
			{:else}
				<p class="hc-quiet empty">{status ? 'No devices in the network yet. Add one.' : 'Asking Z-Wave JS…'}</p>
			{/each}
		</div>
	</section>

	<section class="cards">
		<div class="hc-card">
			<h2>Controller</h2>
			<dl>
				<dt>Stick</dt>
				<dd>{[c.model, c.location].filter(Boolean).join(' · ') || '—'}</dd>
				<dt>Home ID</dt>
				<dd class="hc-num">{c.home_id ?? '—'}</dd>
				<dt>Region</dt>
				<dd>{c.rf_region ?? '—'}</dd>
				<dt>Firmware</dt>
				<dd>{c.firmware_version ?? '—'}{c.sdk_version ? ` · SDK ${c.sdk_version}` : ''}</dd>
				<dt>Z-Wave JS</dt>
				<dd>{status?.connection?.server_version ?? '—'}{status?.connection?.driver_version ? ` · driver ${status.connection.driver_version}` : ''}</dd>
			</dl>
		</div>
		<div class="hc-card">
			<h2>Network</h2>
			<div class="item">
				<strong>Rebuild routes</strong>
				<span class="hc-quiet">Each device finds its best path to the controller again. Useful after moving devices. Takes a few minutes; battery devices update when they next wake.</span>
			</div>
			{#if rebuild && !rebuild.done}
				<div class="bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={rebuild.percent}><div style:width={`${rebuild.percent}%`}></div></div>
				<span class="hc-quiet">{rebuild.message}</span>
				<span class="hc-row"><button class="hc-pill" onclick={stopRebuild}>Stop</button></span>
			{:else}
				{#if rebuild?.done}<span class="ok">{rebuild.message}</span>{/if}
				{#if admin}<span class="hc-row"><button class="hc-pill" disabled={!connected} onclick={rebuildRoutes}>Rebuild routes</button></span>{/if}
			{/if}
			<div class="item rule">
				<strong>Back up the controller</strong>
				<span class="hc-quiet">Saves the stick’s memory — every device and its pairing — to a file. Keep it somewhere safe; it rebuilds the network on a new stick.</span>
			</div>
			{#if backingUp}<span class="hc-quiet">{backingUp}</span>{/if}
			{#if backedUp}<span class="ok">{backedUp}</span>{/if}
			{#if admin}<span class="hc-row"><button class="hc-pill" disabled={!connected || !!backingUp} onclick={backup}>Download a backup</button></span>{/if}
		</div>
	</section>
</div>

{#if adding}
	<AddDevice
		{hc}
		replace={adding.replace}
		onclose={() => {
			adding = null;
			void readStatus();
		}}
	/>
{/if}
{#if removing}
	<RemoveDevice
		{hc}
		onclose={() => {
			removing = false;
			void readStatus();
		}}
	/>
{/if}

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 22px;
		container-type: inline-size;
	}
	.top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
	}
	.status {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 10px;
		border-radius: 11px;
		font-weight: 600;
		font-size: 14px;
	}
	.pill.good {
		background: light-dark(#e6f0e1, #1f3a26);
		color: light-dark(#2f5a27, #8fd19e);
	}
	.pill.warn {
		background: var(--lit-bg);
		color: var(--lit-ink);
	}
	.pill.bad {
		background: var(--alert-bg, var(--lit-bg));
		color: var(--alert);
	}
	.pill .hc-dot {
		width: 8px;
		height: 8px;
	}
	.status .hc-quiet {
		font-size: 14px;
	}
	.hc-pill.big {
		min-height: 44px;
		padding: 0 16px;
	}
	.note {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 14px 18px;
		border-radius: var(--radius, 14px);
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
	}
	.note.bad {
		background: color-mix(in srgb, var(--alert-bg, var(--lit-bg)) 55%, var(--surface));
		border-color: var(--alert-line, var(--card-line));
	}
	.note .words {
		flex: 1 1 320px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.note strong {
		font-weight: 600;
	}
	.note .hc-quiet {
		font-size: 14px;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.ok {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.sect {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		justify-content: space-between;
		gap: 4px 12px;
	}
	h2 {
		margin: 0;
		font-size: 20px;
		font-weight: 600;
	}
	.hc-card h2 {
		font-size: 18px;
	}
	.head .hc-quiet {
		font-size: 14px;
	}
	.table {
		display: flex;
		flex-direction: column;
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
		overflow: hidden;
	}
	.tr {
		display: grid;
		grid-template-columns: minmax(0, 2.2fr) minmax(0, 1.3fr) minmax(0, 1.2fr) minmax(0, 1.1fr) minmax(0, 1fr) minmax(0, 0.9fr);
		gap: 12px;
		align-items: center;
		min-height: 56px;
		padding: 0 16px;
		border: none;
		border-top: 1px solid var(--card-line);
		background: none;
		color: inherit;
		text-align: left;
		font-size: 14px;
	}
	.tr:hover {
		background: color-mix(in srgb, var(--lit-bg) 25%, var(--surface));
	}
	.tr.th {
		min-height: 0;
		padding: 10px 16px;
		border-top: none;
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--ink-muted);
	}
	.tr.th:hover {
		background: none;
	}
	.tr.dead .now {
		color: var(--alert);
		font-weight: 600;
	}
	.dev {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.dev b {
		font-weight: 600;
		font-size: 15px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.dev small {
		font-size: 12px;
		color: var(--ink-muted);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.pow.low {
		color: var(--alert);
		font-weight: 600;
	}
	.sig {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.sig .hc-dot {
		width: 8px;
		height: 8px;
	}
	.heard {
		font-size: 12px;
		color: var(--ink-muted);
	}
	.empty {
		padding: 16px;
	}
	.cards {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 320px), 1fr));
		gap: 16px;
		align-items: start;
	}
	dl {
		display: grid;
		grid-template-columns: 110px minmax(0, 1fr);
		row-gap: 6px;
		margin: 0;
		font-size: 14px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.item {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.item strong {
		font-weight: 600;
	}
	.item .hc-quiet {
		font-size: 14px;
	}
	.item.rule {
		border-top: 1px solid var(--card-line);
		padding-top: 12px;
	}
	.bar {
		height: 8px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--track-off, #ddd) 60%, var(--surface));
		overflow: hidden;
	}
	.bar div {
		height: 100%;
		background: var(--accent);
		transition: width 0.4s;
	}
	/* A phone: each device is two lines — name and what it's doing, then its
	   room, power, signal and when it was heard. */
	@container (max-width: 640px) {
		.tr.th,
		.room,
		.heard {
			display: none;
		}
		.tr {
			grid-template-columns: minmax(0, 1fr) auto;
			row-gap: 2px;
			column-gap: 10px;
			padding: 10px 14px;
		}
		.tr:nth-child(2) {
			border-top: none;
		}
		.now,
		.sig {
			justify-self: end;
			text-align: right;
		}
		.pow,
		.sig {
			font-size: 12px;
			color: var(--ink-muted);
		}
		.pow.low {
			color: var(--alert);
		}
	}
</style>
