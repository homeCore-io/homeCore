<!-- Artwork: the real picture when there is one, else a quiet tile. -->
<script>
	let { src = null, size = 56, radius = 10, fill = false, label = '' } = $props();
	let failed = $state('');
	const shown = $derived(src && failed !== src ? src : null);
</script>

<span class="art" class:fill style:width={fill ? '100%' : `${size}px`} style:height={fill ? 'auto' : `${size}px`} style:border-radius={`${radius}px`}>
	{#if shown}
		<img src={shown} alt={label} loading="lazy" onerror={() => (failed = src)} />
	{/if}
</span>

<style>
	.art {
		flex: none;
		display: block;
		overflow: hidden;
		background: color-mix(in srgb, var(--track-off) 35%, var(--surface));
	}
	.art.fill {
		aspect-ratio: 1;
	}
	img {
		display: block;
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
</style>
