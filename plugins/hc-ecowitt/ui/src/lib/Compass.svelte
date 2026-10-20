<!-- The wind: where it comes from now (the arrow), over ten minutes (the
     faint one), and how hard, in the middle. -->
<script>
	import { compass, n, num, u } from './weather.js';
	let { dir, avg, speed, gust } = $props();
	const at = (deg) => `rotate(${num(deg) ?? 0} 50 50)`;
</script>

<figure class="compass" aria-label={`Wind from ${compass(dir)} at ${n(speed, 'm/s')} ${u('m/s')}, gusting ${n(gust, 'm/s')}`}>
	<svg viewBox="0 0 100 100" aria-hidden="true">
		<circle cx="50" cy="50" r="44" class="ring" />
		{#each Array.from({ length: 36 }, (_, i) => i * 10) as d (d)}
			<line x1="50" y1="8" x2="50" y2={d % 90 === 0 ? 14 : 11} class="tick" transform={at(d)} />
		{/each}
		<text x="50" y="23" class="pt">N</text>
		<text x="79" y="53" class="pt">E</text>
		<text x="50" y="83" class="pt">S</text>
		<text x="21" y="53" class="pt">W</text>
		{#if num(avg) !== undefined}
			<path d="M50 6 L45 18 L55 18 Z" class="avg" transform={at(avg)} />
		{/if}
		{#if num(dir) !== undefined}
			<path d="M50 4 L43 20 L50 16 L57 20 Z" class="now" transform={at(dir)} />
		{/if}
	</svg>
	<figcaption>
		<b class="hc-num">{n(speed, 'm/s')}</b><span>{u('m/s')}</span>
		<small>{compass(dir)} · gust {n(gust, 'm/s')}</small>
	</figcaption>
</figure>

<style>
	.compass {
		position: relative;
		margin: 0;
		width: 100%;
		max-width: 180px;
		aspect-ratio: 1;
	}
	svg {
		width: 100%;
		height: 100%;
	}
	.ring {
		fill: var(--surface-2);
		stroke: var(--card-line);
	}
	.tick {
		stroke: var(--ink-muted);
		stroke-width: 0.6;
	}
	.pt {
		font-size: 8px;
		font-weight: 700;
		fill: var(--ink-muted);
		text-anchor: middle;
		dominant-baseline: middle;
	}
	.now {
		fill: var(--accent);
	}
	.avg {
		fill: var(--ink-muted);
		opacity: 0.35;
	}
	figcaption {
		position: absolute;
		inset: 0;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		line-height: 1.1;
	}
	b {
		font-size: 26px;
	}
	span {
		font-size: 12px;
		color: var(--ink-muted);
	}
	small {
		margin-top: 4px;
		font-size: 11px;
		color: var(--ink-muted);
	}
</style>
