<!-- The few marks the Roku screens draw. -->
<script>
	let { name, size = 18 } = $props();
</script>

<svg width={size} height={size} viewBox="0 0 22 22" aria-hidden="true">
	{#if name === 'power'}
		<path
			d="M11 3v8M6.5 6.5a6.5 6.5 0 1 0 9 0"
			fill="none"
			stroke="currentColor"
			stroke-width="1.8"
			stroke-linecap="round"
		/>
	{:else if name === 'play-pause'}
		<path d="M3 5l7 6-7 6z" fill="currentColor" /><rect
			x="12.5"
			y="5"
			width="2.6"
			height="12"
			rx="0.8"
			fill="currentColor"
		/><rect x="16.6" y="5" width="2.6" height="12" rx="0.8" fill="currentColor" />
	{:else if name === 'rewind'}
		<path d="M11 6l-7 5 7 5zM19 6l-7 5 7 5z" fill="currentColor" />
	{:else if name === 'forward'}
		<path d="M3 6l7 5-7 5zM11 6l7 5-7 5z" fill="currentColor" />
	{:else if name === 'replay'}
		<path
			d="M5 8a7 7 0 1 1-1 5M5 3v5h5"
			fill="none"
			stroke="currentColor"
			stroke-width="1.8"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
	{:else if name === 'up'}
		<path
			d="M5 14l6-6 6 6"
			fill="none"
			stroke="currentColor"
			stroke-width="2"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
	{:else if name === 'down'}
		<path
			d="M5 8l6 6 6-6"
			fill="none"
			stroke="currentColor"
			stroke-width="2"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
	{:else if name === 'left'}
		<path
			d="M14 5l-6 6 6 6"
			fill="none"
			stroke="currentColor"
			stroke-width="2"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
	{:else if name === 'right'}
		<path
			d="M8 5l6 6-6 6"
			fill="none"
			stroke="currentColor"
			stroke-width="2"
			stroke-linecap="round"
			stroke-linejoin="round"
		/>
	{/if}
</svg>

<style>
	svg {
		display: block;
		flex: none;
	}
</style>
