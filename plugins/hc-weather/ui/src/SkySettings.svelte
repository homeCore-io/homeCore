<svelte:options
	customElement={{
		tag: 'hc-weather-sky-settings',
		props: { host: { type: 'Object' }, config: { type: 'Object' } }
	}}
/>

<!-- "Weather, animated" settings: which place, and a name for it on the
     card. With one place, only the name. -->
<script>
	let { host: hc, config = {} } = $props();
	const me = $host();
	$effect(() => {
		const root = me?.shadowRoot;
		const sheet = hc?.theme?.sheet;
		if (root && sheet && !root.adoptedStyleSheets.includes(sheet))
			root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
	});

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list ?? [])));
	const places = $derived(devices.filter((d) => d.device_type === 'weather'));
	const label = (d) => (hc.devices.label(d) ?? d.name).replace(/ weather$/i, '');

	// A working copy: the config handed in lags a moment behind each save.
	let draft = $state(null);
	const cfg = $derived(draft ?? { name: '', ...config });
	function save(next) {
		draft = next;
		me.dispatchEvent(new CustomEvent('change', { detail: { config: next } }));
	}
</script>

<div class="settings">
	{#if places.length > 1}
		<label class="hc-field"
			><span class="hc-label">Place</span>
			<select value={cfg.device_id ?? places[0]?.device_id} onchange={(e) => save({ ...cfg, device_id: e.currentTarget.value })}>
				{#each places as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			</select></label
		>
	{/if}
	<label class="hc-field"
		><span class="hc-label">Name on the card</span>
		<input
			value={cfg.name ?? ''}
			placeholder={places[0] ? label(places.find((d) => d.device_id === cfg.device_id) ?? places[0]) : 'Home'}
			onchange={(e) => save({ ...cfg, name: e.currentTarget.value })}
		/></label
	>
	<p class="hc-quiet">The scene follows the weather there now. It stands still for anyone whose device asks for less motion.</p>
</div>

<style>
	.settings {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	p {
		margin: 0;
		font-size: 13px;
	}
</style>
