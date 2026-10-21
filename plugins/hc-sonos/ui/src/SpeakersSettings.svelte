<svelte:options
	customElement={{
		tag: 'hc-sonos-speakers-settings',
		props: { host: { type: 'Object' }, config: { type: 'Object' } }
	}}
/>

<!-- "Sonos speakers" settings: its name, and which speakers — none ticked
     shows every one. -->
<script>
	import { adopt, speakersOf } from './lib/sonos.js';

	let { host: hc, config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const all = $derived(speakersOf(devices, label));

	// A working copy: the config handed in lags a moment behind each save.
	let draft = $state(null);
	const cfg = $derived(draft ?? { name: '', ...config });
	const picked = $derived(Array.isArray(cfg.speakers) ? cfg.speakers : []);
	function save(next) {
		draft = next;
		me.dispatchEvent(new CustomEvent('change', { detail: { config: next } }));
	}
	const toggle = (id) => save({ ...cfg, speakers: picked.includes(id) ? picked.filter((x) => x !== id) : [...picked, id] });
</script>

<div class="settings">
	<label class="hc-field"
		><span class="hc-label">Name</span>
		<input value={cfg.name ?? ''} placeholder="Speakers" onchange={(e) => save({ ...cfg, name: e.currentTarget.value })} /></label
	>
	<div class="box">
		<span class="title">Which speakers</span>
		{#each all as d (d.device_id)}
			<label class="check"><input type="checkbox" checked={picked.includes(d.device_id)} onchange={() => toggle(d.device_id)} /><span>{label(d)}</span></label>
		{/each}
		<span class="hc-quiet">{picked.length ? 'The card shows these.' : 'None ticked: the card shows every speaker.'}</span>
	</div>
</div>

<style>
	.settings {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.box {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 16px;
		border-radius: var(--radius, 14px);
		border: 1px solid var(--card-line);
		background: var(--surface);
	}
	.title {
		font-weight: 600;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 36px;
		font-size: 14px;
	}
	.hc-quiet {
		font-size: 12px;
	}
</style>
