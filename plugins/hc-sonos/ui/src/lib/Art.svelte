<!-- Artwork: the real picture when there is one, else a quiet tile with a
     music note, so a station with no picture isn't a blank square (#28). -->
<script>
	let { src = null, size = 56, radius = 10, fill = false, label = '' } = $props();
	let failed = $state('');
	const shown = $derived(src && failed !== src ? src : null);
</script>

<span class="art" class:fill style:width={fill ? '100%' : `${size}px`} style:height={fill ? 'auto' : `${size}px`} style:border-radius={`${radius}px`}>
	{#if shown}
		<img src={shown} alt={label} loading="lazy" onerror={() => (failed = src)} />
	{:else}
		<svg class="note" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"
			><path d="M9 18V5l12-2v13" /><circle cx="6" cy="18" r="3" /><circle cx="18" cy="16" r="3" /></svg
		>
	{/if}
</span>

<style>
	.art {
		flex: none;
		overflow: hidden;
		background: color-mix(in srgb, var(--track-off) 35%, var(--surface));
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--ink-muted);
	}
	.note {
		width: 36%;
		height: 36%;
		max-width: 40px;
		max-height: 40px;
		opacity: 0.6;
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
