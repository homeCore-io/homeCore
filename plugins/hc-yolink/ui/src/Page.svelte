<svelte:options customElement={{ tag: 'hc-yolink-page', props: { host: { type: 'Object' } } }} />

<!-- YoLink's plugin page: whether every device reports and the live feed is
     up; a low battery; the water (every leak sensor, dry or wet, and when it
     last checked in); temperatures with what homeCore tells you about them;
     doors; locks; switches and the rest; batteries lowest first; and the
     sensors that have gone quiet. -->
<script>
	import { untrack } from 'svelte';
	import {
		adopt,
		ago,
		batteries,
		batteryLow,
		batteryShort,
		BATTERY_WORDS,
		day,
		failed,
		hubLine,
		isDoor,
		isLeak,
		isLock,
		isRelay,
		isTemp,
		isVibration,
		leakAutomations,
		leakLine,
		listWords,
		lockState,
		lockSubline,
		quarters,
		quietAfter,
		reporting,
		settingsHref,
		switchWords,
		temp,
		water
	} from './lib/yolink.js';
	import { readRules, rulesFor, tellsWords } from './lib/tell.js';

	let { host: hc } = $props();
	let alerts = $state();
	$effect(() => hc?.alerts?.subscribe((list) => (alerts = list)));
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const byName = (a, b) => label(a).localeCompare(label(b));
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));

	// Units: redraw when the person changes them.
	let unitsTick = $state(0);
	$effect(() => hc?.units?.subscribe?.(() => unitsTick++));

	let status = $state(null);
	let tick = $state(Date.now());
	async function readStatus() {
		const r = await hc.query('status');
		if (r.ok && r.data?.status !== 'error') status = r.data;
	}
	$effect(() => {
		if (!hc) return;
		void readStatus();
		const t = setInterval(() => {
			tick = Date.now();
			void readStatus();
		}, 15000);
		return () => clearInterval(t);
	});

	// The house's alert rules and automations, where this person may read them.
	let rules = $state([]);
	let automations = $state([]);
	async function readHouse() {
		rules = (await readRules(hc)).rules;
		try {
			const r = await hc.fetch('/automations');
			const body = r.ok ? await r.json() : [];
			automations = Array.isArray(body) ? body : (body.rules ?? []);
		} catch {
			automations = [];
		}
	}
	$effect(() => {
		if (hc) void readHouse();
	});

	// When each vibration sensor last shook (again when one shakes).
	let shook = $state({});
	const shakeKey = $derived(devices.filter(isVibration).map((d) => `${d.device_id}:${d.attributes.vibration}`).join(','));
	$effect(() => {
		if (!hc || !shakeKey) return;
		for (const d of untrack(() => devices.filter(isVibration))) {
			void hc.devices.history(d.device_id, 'vibration', { limit: 500 }).then((r) => {
				if (!r.ok) return;
				const last = [...r.data].reverse().find((p) => p.value === true);
				shook = { ...shook, [d.device_id]: last?.at ?? null };
			});
		}
	});

	const all = $derived(devices);
	const quietOnes = $derived(all.filter((d) => !reporting(d)).sort(byName));
	const lows = $derived(all.filter((d) => batteryLow(d, alerts)).sort(byName));
	const w = $derived(water(all, tick));
	const leaks = $derived([...w.leaks].sort(byName));
	const valveAutos = $derived(leakAutomations(automations, w.leaks.map((d) => d.device_id)));
	const temps = $derived(all.filter(isTemp).sort((a, b) => (a.attributes.humidity === undefined ? -1 : 0) - (b.attributes.humidity === undefined ? -1 : 0) || byName(a, b)));
	const doors = $derived(all.filter(isDoor).sort((a, b) => (b.attributes.open === true) - (a.attributes.open === true) || byName(a, b)));
	const opened = $derived(doors.filter((d) => d.attributes.open === true).length);
	const locks = $derived(all.filter(isLock).sort(byName));
	const others = $derived([...all.filter(isRelay), ...all.filter(isVibration)].sort(byName));
	const batts = $derived(batteries(all));
	const full = $derived(all.filter((d) => quarters(d) === 4).length);

	const reportingCount = $derived(all.filter(reporting).length);
	const allReporting = $derived(all.length > 0 && reportingCount === all.length);
	const feedUp = $derived(!!status?.feed?.connected);
	const tone = $derived(!status ? '' : !feedUp || quietOnes.length ? 'bad' : 'good');

	let busy = $state(false);
	let error = $state('');
	let said = $state('');
	async function checkAll() {
		error = '';
		busy = true;
		const r = await hc.action('poll_now');
		busy = false;
		error = failed(r);
		if (!error) said = r.data?.already ? 'Already checking — a few seconds per device.' : 'Checking every device, a few seconds apart…';
		setTimeout(() => (said = ''), 8000);
		setTimeout(readStatus, 5000);
	}

	function otherWords(d) {
		if (isVibration(d)) {
			const at = shook[d.device_id];
			const now = d.attributes.vibration === true ? 'Shaking' : 'Quiet';
			return at ? `${now} · last shook ${day(at, tick)}` : now;
		}
		return switchWords(d);
	}
	const open = (d) => hc.ui.openDevice(d.device_id);
</script>

<div class="page">
	<header class="top">
		<span class="pill {tone}"
			><span class="hc-dot {tone}"></span>{!status
				? 'Asking the hub…'
				: !all.length
					? 'No devices yet'
					: allReporting
						? `${all.length} devices · all reporting`
						: `${reportingCount} of ${all.length} reporting`}</span
		>
		<span class="hc-quiet line">{status ? hubLine(status, tick) : ''}</span>
		<span class="hc-row">
			<button class="hc-pill big" disabled={busy || !status?.configured || status?.polling} onclick={checkAll}>{status?.polling ? 'Checking…' : 'Check every device now'}</button>
			{#if admin}<button class="hc-pill big" onclick={() => hc.ui.navigate(settingsHref(hc))}>Settings</button>{/if}
		</span>
	</header>
	{#if said}<p class="hc-quiet said" role="status">{said}</p>{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	{#if status && !feedUp && status.configured}
		<div class="note bad">
			<div class="words">
				<strong>The hub’s live events are down</strong>
				<span class="hc-quiet">Devices aren’t updating until it reconnects — it tries every few seconds, then checks every device to catch up.{status.feed?.last_error ? ` (${status.feed.last_error})` : ''}</span>
			</div>
		</div>
	{/if}
	{#each lows as d (d.device_id)}
		<div class="note">
			<div class="words">
				<strong>{label(d)}’s battery is low</strong>
				<span class="hc-quiet"
					>YoLink reports {quarters(d) ?? '1'} of 4.{isLock(d)
						? ' A lock with flat batteries can’t be locked from homeCore — change them soon.'
						: isLeak(d)
							? ' A leak sensor with a flat battery can’t tell you about water — change it soon.'
							: ' Change it soon.'}</span
				>
			</div>
			<button class="link" onclick={() => open(d)}>Open {isLock(d) ? 'the lock' : 'it'} ›</button>
		</div>
	{/each}

	{#if leaks.length}
		<section class="sect">
			<div class="head">
				<h2>Water</h2>
				<span class="sum" class:wet={w.wet.length} class:dim={!w.wet.length && w.quiet.length}
					>{w.wet.length
						? `Wet: ${listWords(w.wet.map(label))}`
						: `${w.total} leak sensor${w.total === 1 ? '' : 's'} · ${w.quiet.length ? `${w.total - w.quiet.length} dry, ${w.quiet.length} not reporting` : 'all dry'}`}{valveAutos.length
						? ` · ${listWords(valveAutos.map((a) => `“${a.name}”`))} ${valveAutos.length === 1 ? 'is' : 'are'} on`
						: ''}</span
				>
			</div>
			<div class="tiles">
				{#each leaks as d (d.device_id)}
					<button class="tile" class:wet={d.attributes.water_detected === true} class:quiet={!reporting(d)} onclick={() => open(d)}>
						<span class="nm">{label(d)}</span>
						<span class="hc-quiet st">{leakLine(d, tick)}</span>
					</button>
				{/each}
			</div>
		</section>
	{/if}

	{#if temps.length}
		<section class="sect">
			<h2>Temperatures</h2>
			<div class="tablebox">
				<table>
					<thead><tr><th>Sensor</th><th>Now</th><th>Humidity</th><th>Tells you when</th><th>Battery</th></tr></thead>
					<tbody>
						{#each temps as d (d.device_id)}
							{@const tells = (void unitsTick, tellsWords(hc, rulesFor(rules, d.device_id)))}
							<tr onclick={() => open(d)}>
								<td class="nm">{label(d)}</td>
								<td class="hc-num now" class:off={!reporting(d)}>{(void unitsTick, reporting(d) ? temp(hc, d.attributes.temperature) : 'Not reporting')}</td>
								<td class="dim">{d.attributes.humidity !== undefined ? `${Math.round(d.attributes.humidity)} %` : '—'}</td>
								<td class="dim">{tells || '—'}</td>
								<td class="dim" class:low={batteryLow(d, alerts)}>{batteryShort(d)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</section>
	{/if}

	<div class="two">
		{#if doors.length}
			<section class="sect">
				<div class="head">
					<h2>Doors</h2>
					<span class="sum" class:warn={opened}>{opened ? `${opened} open` : 'all closed'}</span>
				</div>
				<div class="list">
					{#each doors as d (d.device_id)}
						<button class="row" onclick={() => open(d)}>
							<span>{label(d)}</span>
							<span class:open={d.attributes.open === true} class="dim">{!reporting(d) ? 'Not reporting' : d.attributes.open === true ? 'Open' : 'Closed'}</span>
						</button>
					{/each}
				</div>
			</section>
		{/if}

		<div class="stack">
			{#if locks.length}
				<section class="sect">
					<h2>Locks</h2>
					<div class="list">
						{#each locks as d (d.device_id)}
							<button class="row tall" onclick={() => open(d)}>
								<span class="two-line"><b>{label(d)}</b><small>{lockSubline(d, alerts)}</small></span>
								<span class="lk" class:unlocked={d.attributes.locked !== true}>{reporting(d) ? lockState(d) : 'Not reporting'}</span>
							</button>
						{/each}
					</div>
				</section>
			{/if}
			{#if others.length}
				<section class="sect">
					<h2>Switches and other</h2>
					<div class="list">
						{#each others as d (d.device_id)}
							<button class="row" onclick={() => open(d)}>
								<span>{label(d)}</span>
								<span class="dim">{reporting(d) ? otherWords(d) : 'Not reporting'}</span>
							</button>
						{/each}
					</div>
				</section>
			{/if}
		</div>
	</div>

	{#if batts.length}
		<section class="sect">
			<div class="head">
				<h2>Batteries</h2>
				<span class="hc-quiet small">Lowest first. YoLink counts in quarters, so these are 4, 3, 2, 1 of 4 — not exact percentages.</span>
			</div>
			<div class="tiles wide">
				{#each batts as d (d.device_id)}
					{@const q = quarters(d)}
					<button class="tile batt" onclick={() => open(d)}>
						<span class="bars" aria-hidden="true">{#each [0, 1, 2, 3] as i (i)}<span class:on={i < q} class:bad={q <= 1}></span>{/each}</span>
						<span class="words2"><span class="nm">{label(d)}</span><span class="bn" class:low={batteryLow(d, alerts)}>{batteryLow(d, alerts) ? 'Low — change soon' : BATTERY_WORDS[q]}</span></span>
					</button>
				{/each}
			</div>
			{#if full}<span class="hc-quiet small">The other {full} {full === 1 ? 'is' : 'are'} full.</span>{/if}
		</section>
	{/if}

	<section class="hc-card quiet-box">
		<strong>Sensors that have gone quiet</strong>
		{#if quietOnes.length}
			<div class="list flat">
				{#each quietOnes as d (d.device_id)}
					<button class="row" onclick={() => open(d)}>
						<span>{label(d)}</span>
						<span class="bad-t">{d.attributes.last_seen ? `last heard ${ago(d.attributes.last_seen, tick)}` : 'not heard since homeCore started'}</span>
					</button>
				{/each}
			</div>
		{/if}
		<span class="hc-quiet"
			>{quietOnes.length ? '' : 'None. '}A sensor that misses its regular check-in{quietAfter(status) ? ` (${quietAfter(status)} with nothing)` : ''} shows here and goes offline, so a leak sensor with dead batteries is never shown as dry.</span
		>
	</section>
</div>

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
		gap: 12px;
	}
	.top .line {
		flex: 1 1 320px;
		font-size: 14px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		padding: 6px 12px;
		border-radius: 999px;
		font-weight: 600;
		font-size: 14px;
		background: var(--line);
	}
	.pill.good {
		background: light-dark(#e6f0e1, #1f3a26);
		color: light-dark(#2f5a27, #8fd19e);
	}
	.pill.bad {
		background: var(--alert-bg, var(--lit-bg));
		color: var(--alert);
	}
	.pill .hc-dot {
		width: 8px;
		height: 8px;
	}
	.hc-pill.big {
		min-height: 44px;
		padding: 0 16px;
	}
	.said {
		margin: -10px 0 0;
		font-size: 14px;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.note {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
		padding: 14px 18px;
		border-radius: var(--radius, 14px);
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
	}
	.note.bad {
		background: color-mix(in srgb, var(--alert-bg, var(--lit-bg)) 55%, var(--surface));
	}
	.note .words {
		flex: 1 1 360px;
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
	.sect {
		display: flex;
		flex-direction: column;
		gap: 12px;
		min-width: 0;
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
	.sum {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.sum.wet {
		color: var(--alert);
	}
	.sum.warn {
		color: var(--lit-ink, #7a5a10);
	}
	.sum.dim {
		color: var(--ink-muted);
	}
	.small {
		font-size: 13px;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 230px), 1fr));
		gap: 10px;
	}
	.tiles.wide {
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 250px), 1fr));
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 12px 14px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		text-align: left;
		color: inherit;
		min-width: 0;
	}
	.tile.wet {
		background: color-mix(in srgb, var(--alert-bg, #fbe9e2) 70%, var(--surface));
		border-color: var(--alert-line, var(--card-line));
	}
	.tile.wet .st,
	.tile.quiet .st {
		color: var(--alert);
		font-weight: 600;
	}
	.nm {
		font-weight: 600;
		font-size: 15px;
	}
	.st {
		font-size: 13px;
	}
	.tablebox {
		overflow-x: auto;
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: 14px;
	}
	table {
		width: 100%;
		min-width: 640px;
		border-collapse: collapse;
		font-size: 15px;
	}
	th {
		text-align: left;
		padding: 12px 8px;
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--ink-muted);
	}
	th:first-child,
	td:first-child {
		padding-left: 16px;
	}
	th:last-child,
	td:last-child {
		padding-right: 16px;
	}
	td {
		padding: 12px 8px;
		border-top: 1px solid var(--card-line);
	}
	tr {
		cursor: pointer;
	}
	tbody tr:hover {
		background: color-mix(in srgb, var(--lit-bg) 25%, var(--surface));
	}
	td.nm {
		font-weight: 600;
	}
	td.now {
		font-size: 14px;
	}
	td.now.off {
		color: var(--alert);
		font-family: inherit;
	}
	.dim {
		color: var(--ink-muted);
	}
	.low {
		color: var(--alert) !important;
		font-weight: 600;
	}
	.two {
		display: flex;
		flex-wrap: wrap;
		gap: 22px;
	}
	.two > * {
		flex: 1 1 420px;
		min-width: 0;
	}
	.stack {
		display: flex;
		flex-direction: column;
		gap: 22px;
	}
	.list {
		display: flex;
		flex-direction: column;
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: 14px;
		overflow: hidden;
	}
	.list.flat {
		border-radius: 10px;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 48px;
		padding: 0 16px;
		border: none;
		border-top: 1px solid var(--card-line);
		background: none;
		color: inherit;
		font-size: 15px;
		text-align: left;
	}
	.row:first-child {
		border-top: none;
	}
	.row:hover {
		background: color-mix(in srgb, var(--lit-bg) 25%, var(--surface));
	}
	.row.tall {
		min-height: 56px;
	}
	.row .open {
		color: var(--lit-ink, #7a5a10);
		font-weight: 600;
	}
	.two-line {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.two-line b {
		font-weight: 600;
	}
	.two-line small {
		font-size: 13px;
		color: var(--ink-muted);
	}
	.lk {
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.lk.unlocked {
		color: var(--alert);
	}
	.batt {
		flex-direction: row;
		align-items: center;
		gap: 12px;
	}
	.bars {
		display: flex;
		gap: 3px;
		flex: none;
	}
	.bars span {
		width: 6px;
		height: 16px;
		border-radius: 2px;
		background: var(--card-line);
	}
	.bars span.on {
		background: var(--ink-muted);
	}
	.bars span.on.bad {
		background: var(--alert);
	}
	.words2 {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-width: 0;
	}
	.bn {
		font-size: 13px;
		color: var(--ink-muted);
	}
	.bn.low {
		color: var(--alert);
		font-weight: 600;
	}
	.quiet-box {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 16px;
	}
	.quiet-box strong {
		font-weight: 600;
	}
	.quiet-box .hc-quiet {
		font-size: 14px;
	}
	.bad-t {
		color: var(--alert);
		font-size: 14px;
	}
</style>
