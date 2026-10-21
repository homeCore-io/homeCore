<!-- The few marks the Sonos screens draw, as the mockup draws them. -->
<script>
	let { name, size = 18 } = $props();
</script>

<svg width={size} height={size} viewBox={name === 'play' || name === 'pause' ? '0 0 16 16' : '0 0 18 18'} aria-hidden="true">
	{#if name === 'play'}
		<path d="M5 3l8 5-8 5z" fill="currentColor" />
	{:else if name === 'pause'}
		<rect x="3.5" y="3" width="3" height="10" rx="1" fill="currentColor" /><rect x="9.5" y="3" width="3" height="10" rx="1" fill="currentColor" />
	{:else if name === 'prev'}
		<path d="M5 4v10M14 4l-7 5 7 5z" fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
	{:else if name === 'next'}
		<path d="M13 4v10M4 4l7 5-7 5z" fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
	{:else if name === 'shuffle'}
		<path d="M2 5h3l7 8h4M2 13h3l2-2.3M11 6.3L12 5h4M14 3l2 2-2 2M14 11l2 2-2 2" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
	{:else if name === 'repeat'}
		<path d="M3 8V7a2 2 0 012-2h9l-2-2M15 10v1a2 2 0 01-2 2H4l2 2" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
	{:else if name === 'speaker'}
		<path d="M3 7h3l4-3v10l-4-3H3z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" />
	{:else if name === 'volume'}
		<path d="M3 7h3l4-3v10l-4-3H3zM13 6.5a4 4 0 010 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" />
	{:else if name === 'muted'}
		<path d="M3 7h3l4-3v10l-4-3H3zM12.5 7l4 4M16.5 7l-4 4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" />
	{/if}
</svg>

<style>
	svg {
		display: block;
		flex: none;
	}
</style>
