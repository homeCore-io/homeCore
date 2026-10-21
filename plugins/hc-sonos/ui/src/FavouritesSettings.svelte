<svelte:options
	customElement={{
		tag: 'hc-sonos-favourites-settings',
		props: { host: { type: 'Object' }, config: { type: 'Object' } }
	}}
/>

<!-- "Sonos favourites" settings: which speaker a tap plays on. -->
<script>
	import { adopt, speakersOf } from './lib/sonos.js';

	let { host: hc, config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const all = $derived(speakersOf(devices, label));

	let draft = $state(null);
	const cfg = $derived(draft ?? { ...config });
	function save(next) {
		draft = next;
		me.dispatchEvent(new CustomEvent('change', { detail: { config: next } }));
	}
</script>

<div class="settings">
	<label class="hc-field"
		><span class="hc-label">Play on</span>
		<select value={cfg.speaker ?? 'playing'} onchange={(e) => save({ ...cfg, speaker: e.currentTarget.value })}>
			{#each all as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			<option value="playing">Whichever is playing</option>
		</select></label
	>
	<label class="hc-field"
		><span class="hc-label">Name</span>
		<input value={cfg.name ?? ''} placeholder="Favourites" onchange={(e) => save({ ...cfg, name: e.currentTarget.value })} /></label
	>
</div>

<style>
	.settings {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 14px 16px;
		border-radius: var(--radius, 14px);
		border: 1px solid var(--card-line);
		background: var(--surface);
	}
	.hc-field .hc-label {
		font-size: 13px;
		color: var(--ink);
	}
	.hc-field select,
	.hc-field input {
		min-height: 44px;
		border-radius: var(--radius-chip, 11px);
		padding: 0 12px;
	}
</style>
