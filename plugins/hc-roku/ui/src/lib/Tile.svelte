<!-- A channel's tile: its own icon from the Roku (kept by the plugin), else
     its colour and short name until the icon arrives. -->
<script>
	import { iconUrl, short, tint } from './roku.js';

	let { hc, app, label = '', radius = 10, lit = false } = $props();
	let failed = $state('');
	const src = $derived(iconUrl(hc, app));
	const shown = $derived(src && failed !== src ? src : null);
</script>

<span
	class="tile"
	class:lit
	style:border-radius={`${radius}px`}
	style:background={shown ? 'var(--surface)' : app ? tint(app.name) : 'var(--track-off)'}
>
	{#if shown}
		<img src={shown} alt="" loading="lazy" onerror={() => (failed = src)} />
	{:else}
		<span class="word">{label || short(app?.name)}</span>
	{/if}
</span>

<style>
	.tile {
		display: flex;
		align-items: center;
		justify-content: center;
		aspect-ratio: 4 / 3;
		width: 100%;
		overflow: hidden;
		box-sizing: border-box;
		color: #ffffff;
	}
	.tile.lit {
		outline: 3px solid #e3b56a;
		outline-offset: 1px;
	}
	img {
		display: block;
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.word {
		padding: 4px;
		font-size: 11px;
		font-weight: 700;
		line-height: 1.15;
		text-align: center;
		overflow-wrap: anywhere;
	}
</style>
