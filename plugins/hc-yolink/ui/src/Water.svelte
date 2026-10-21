<svelte:options customElement={{ tag: 'hc-yolink-water', props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } } }} />

<!-- "Water", a card for a page: all dry, or which are wet and since when
     (and what ran because of it); and whether every leak sensor checked in
     within the hour — one that has gone quiet is named, never counted dry. -->
<script>
	import { untrack } from 'svelte';
	import { adopt, clock, isLeak, leakAutomations, listWords, pageHref, water } from './lib/yolink.js';

	let { host: hc, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);

	let tick = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (tick = Date.now()), 60000);
		return () => clearInterval(t);
	});
	const w = $derived(water(devices, tick));

	// When each wet one got wet (its history), and the automations that start
	// on a leak, with when they last ran.
	let since = $state({});
	let autos = $state([]);
	const wetKey = $derived(w.wet.map((d) => d.device_id).join(','));
	const leakKey = $derived(devices.filter(isLeak).map((d) => d.device_id).join(','));
	$effect(() => {
		if (!hc) return;
		void wetKey;
		void leakKey;
		untrack(() => readAll());
	});
	function readAll() {
		const wetSince = (d, again) =>
			void hc.devices.history(d.device_id, 'water_detected', { limit: 50 }).then((r) => {
				if (!r.ok) return;
				// Homecore may not have kept the newest point yet: a history
				// that ends dry is read again in a moment.
				if (r.data.at(-1)?.value !== true) {
					if (again) setTimeout(() => wetSince(d, false), 3000);
					return;
				}
				const last = [...r.data].reverse().find((p, i, all) => p.value === true && all[i + 1]?.value !== true);
				if (last) since = { ...since, [d.device_id]: last.at };
			});
		for (const d of w.wet) wetSince(d, true);
		void hc
			.fetch('/automations')
			.then((r) => (r.ok ? r.json() : []))
			.then((b) => (autos = leakAutomations(Array.isArray(b) ? b : (b.rules ?? []), devices.filter(isLeak).map((d) => d.device_id))))
			.catch(() => (autos = []));
		if (w.wet.length)
			void hc
				.fetch('/events?type=rule_fired&limit=200')
				.then((r) => (r.ok ? r.json() : []))
				.then((rows) => (fired = Array.isArray(rows) ? rows.map((x) => x.event ?? x) : []))
				.catch(() => (fired = []));
	}
	let fired = $state([]);
	const firstWet = $derived(Object.values(since).sort()[0]);
	// The leak automations that ran since it got wet.
	const ran = $derived.by(() => {
		if (!firstWet) return [];
		const ids = new Set(autos.map((a) => a.id));
		const names = fired.filter((e) => ids.has(e.rule_id) && String(e.timestamp) >= firstWet).map((e) => e.rule_name);
		return [...new Set(names)];
	});
</script>

<div class="hc-tile {form}" class:wet={w.wet.length}>
	<div class="head">
		<span class="t">Water</span>
		<button class="link" onclick={() => hc.ui.navigate(pageHref(hc))}>YoLink ›</button>
	</div>
	{#if !w.total}
		<span class="hc-tile-state">No leak sensors yet.</span>
	{:else if w.wet.length}
		<span class="big bad">Wet: {listWords(w.wet.map((d) => label(d).replace(/ leak sensor$/i, '')))}</span>
		<span class="hc-tile-state">{[firstWet ? `since ${clock(firstWet)}` : '', ran.length ? `${listWords(ran)} ran` : ''].filter(Boolean).join(' · ')}</span>
	{:else}
		<div class="count">
			<span class="big good">{w.quiet.length ? `${w.total - w.quiet.length} dry` : 'All dry'}</span><span class="of">{w.total} sensor{w.total === 1 ? '' : 's'}</span>
		</div>
	{/if}
	{#if w.total}
		<span class="foot" class:bad={w.quiet.length}
			>{w.quiet.length
				? `Not reporting: ${listWords(w.quiet.map(label))}`
				: `Checked in within the last hour: ${w.checkedIn.length === w.total ? `all ${w.total}` : `${w.checkedIn.length} of ${w.total}`}`}</span
		>
	{/if}
</div>

<style>
	.hc-tile {
		height: 100%;
		gap: 10px;
		padding: 16px;
	}
	.hc-tile.wet {
		background: color-mix(in srgb, var(--alert-bg, #fbe9e2) 75%, var(--surface));
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}
	.t {
		font-weight: 600;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		font-size: 13px;
		color: var(--accent);
	}
	.count {
		display: flex;
		align-items: baseline;
		gap: 8px;
		flex-wrap: wrap;
	}
	.big {
		font-size: 26px;
		font-weight: 600;
		line-height: 1.15;
	}
	.wet .big {
		font-size: 22px;
	}
	.good {
		color: light-dark(#2f5a27, #8fd19e);
	}
	.bad {
		color: var(--alert);
	}
	.of {
		color: var(--ink-muted);
		font-size: 14px;
	}
	.foot {
		margin-top: auto;
		font-size: 13px;
		color: var(--ink-muted);
		border-top: 1px solid var(--card-line);
		padding-top: 8px;
	}
	.foot.bad {
		color: var(--alert);
	}
</style>
