<!-- A day of one reading, as a line with its low and high. -->
<script>
	import { fmt } from './weather.js';
	let { points = [], unit = '', digits = 1, label = '' } = $props();
	const W = 300;
	const H = 64;
	const vals = $derived(
		points
			.map((p) => ({ t: Date.parse(p.at), v: Number(p.value) }))
			.filter((p) => Number.isFinite(p.t) && Number.isFinite(p.v))
	);
	const lo = $derived(vals.length ? Math.min(...vals.map((p) => p.v)) : 0);
	const hi = $derived(vals.length ? Math.max(...vals.map((p) => p.v)) : 0);
	const line = $derived.by(() => {
		if (vals.length < 2) return '';
		const t0 = vals[0].t;
		const span = Math.max(1, vals[vals.length - 1].t - t0);
		const range = hi - lo || 1;
		return vals
			.map((p, i) => `${i ? 'L' : 'M'}${(((p.t - t0) / span) * W).toFixed(1)} ${(H - 4 - ((p.v - lo) / range) * (H - 8)).toFixed(1)}`)
			.join(' ');
	});
</script>

<div class="spark">
	<div class="hc-head">
		<span class="hc-label">{label}</span>
		{#if vals.length}<span class="hc-num hc-quiet">{fmt(lo, digits)}–{fmt(hi, digits)}{unit}</span>{/if}
	</div>
	{#if line}
		<svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden="true"><path d={line} /></svg>
	{:else}
		<p class="hc-quiet">Not enough of a day yet.</p>
	{/if}
</div>

<style>
	.spark {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	svg {
		width: 100%;
		height: 64px;
		background: var(--surface-2);
		border-radius: 8px;
	}
	path {
		fill: none;
		stroke: var(--track-on, var(--accent));
		stroke-width: 2;
		vector-effect: non-scaling-stroke;
	}
</style>
