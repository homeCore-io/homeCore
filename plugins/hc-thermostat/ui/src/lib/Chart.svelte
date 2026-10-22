<!-- Today, since midnight: the room's temperature against the target's band,
     with the times the switch was on shaded. From homeCore's own history of
     the thermostat (its `temperature` and `actuator_state`), read through
     host.devices.history. -->
<script>
	import { band, diffText, kindOf, kindWords, midnight, num, targetText } from './thermo.js';
	import { onRuns, pathOf, yRange } from './chart.js';

	let { hc, dev, units, height = 120, small = false } = $props();

	let temps = $state([]);
	let runs = $state([]);
	let now = $state(Date.now());
	let loaded = $state(false);

	$effect(() => {
		const id = dev?.device_id;
		if (!hc || !id) return;
		let gone = false;
		async function load() {
			const from = midnight();
			const [t, s] = await Promise.all([
				hc.devices.history(id, 'temperature', { from, limit: 2000 }),
				hc.devices.history(id, 'actuator_state', { from, limit: 2000 })
			]);
			if (gone) return;
			now = Date.now();
			temps = t.ok
				? t.data
						.map((p) => ({ at: Date.parse(p.at), v: num(p.value) }))
						.filter((p) => p.v !== undefined && Number.isFinite(p.at))
				: [];
			const a = dev?.attributes ?? {};
			const onSince = a.actuator_state === true ? Date.parse(a.on_since ?? '') : NaN;
			runs = s.ok ? onRuns(s.data, Date.parse(from), now, onSince) : [];
			loaded = true;
		}
		void load();
		const t = setInterval(load, 120000);
		return () => {
			gone = true;
			clearInterval(t);
		};
	});

	const start = $derived(Date.parse(midnight(now)));
	// The latest reading counts as now, so the line reaches the right edge.
	const points = $derived.by(() => {
		const live = num(dev?.attributes?.temperature);
		return live === undefined ? temps : [...temps, { at: now, v: live }];
	});
	const b = $derived(band(dev));
	const y = $derived(yRange(points, b));
	const W = 1000;
	const yOf = (v) => height - ((v - y.min) / (y.max - y.min)) * height;
	const xOf = (t) => ((t - start) / Math.max(1, now - start)) * W;
	const line = $derived(pathOf(points, xOf, yOf));
	const k = $derived(kindWords(dev));
</script>

<div class="chart" class:small class:heat={kindOf(dev) === 'heater'} style:height={`${height}px`}>
	<svg viewBox={`0 0 ${W} ${height}`} preserveAspectRatio="none" aria-hidden="true">
		<rect class="band" x="0" width={W} y={yOf(b.high)} height={yOf(b.low) - yOf(b.high)} />
		{#each runs as r (r.from)}
			<rect
				class="run"
				x={xOf(r.from)}
				width={Math.max(2, xOf(r.to) - xOf(r.from))}
				y="0"
				{height}
			/>
		{/each}
		<line class="target" x1="0" x2={W} y1={yOf(b.target)} y2={yOf(b.target)} />
		{#if line}<path class="room" d={line} />{/if}
	</svg>
	{#if !small}
		<span class="lbl tr" style:top={`${Math.max(2, yOf(b.high) - 18)}px`}
			>target {targetText(b.target, units)} ± {diffText(b.half, units)}</span
		>
		<span class="lbl bl">midnight</span>
		<span class="lbl br">now</span>
		{#if loaded && !points.length}<span class="lbl tl">No readings since midnight</span>{/if}
	{/if}
	<span class="sr">Today: the room's temperature, the target band, and when {k.the} was on.</span>
</div>

<style>
	.chart {
		position: relative;
		border-radius: 10px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 70%, var(--surface));
		overflow: hidden;
	}
	.chart.small {
		border-radius: 8px;
	}
	svg {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
	}
	.band {
		fill: light-dark(rgba(31, 90, 133, 0.12), rgba(140, 196, 236, 0.14));
	}
	.run {
		fill: light-dark(rgba(31, 90, 133, 0.18), rgba(140, 196, 236, 0.2));
	}
	.heat .band {
		fill: light-dark(rgba(181, 83, 12, 0.1), rgba(240, 164, 107, 0.14));
	}
	.heat .run {
		fill: light-dark(rgba(181, 83, 12, 0.16), rgba(240, 164, 107, 0.2));
	}
	.heat .target {
		stroke: light-dark(#a3470a, #f0a46b);
	}
	.heat .tr {
		color: light-dark(#a3470a, #f0a46b);
	}
	.target {
		stroke: light-dark(#1f5a85, #8cc4ec);
		stroke-width: 2;
		stroke-dasharray: 8 6;
		vector-effect: non-scaling-stroke;
	}
	.room {
		fill: none;
		stroke: var(--ink);
		stroke-width: 2;
		stroke-linejoin: round;
		vector-effect: non-scaling-stroke;
	}
	.lbl {
		position: absolute;
		font-size: 11px;
		color: var(--ink-muted);
	}
	.tr {
		right: 8px;
		color: light-dark(#1f5a85, #8cc4ec);
	}
	.tl {
		left: 8px;
		top: 6px;
	}
	.bl {
		left: 8px;
		bottom: 6px;
	}
	.br {
		right: 8px;
		bottom: 6px;
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
</style>
