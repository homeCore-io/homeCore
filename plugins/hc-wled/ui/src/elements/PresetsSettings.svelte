<svelte:options
	customElement={{
		tag: 'hc-wled-presets-settings',
		props: { host: { type: 'Object' }, config: { type: 'Object' } }
	}}
/>

<!-- "Quick presets" settings: a title, the controllers it applies to, and
     which presets it offers (from the first controller chosen). -->
<script lang="ts">
	import type { Host } from '#lib/host.js';
	import { devices } from '#lib/devices.svelte.js';
	import { isWled, presetCss, presetsOf } from '#lib/wled.js';
	import { start } from './adopt.js';

	type Config = { device_ids?: string[]; presets?: number[]; title?: string };
	let { host: hc, config = {} }: { host: Host; config?: Config } = $props();
	const me = $host();
	start(me, hc);
	$effect(() => start(me, hc));

	const all = $derived(
		Object.values(devices.byId)
			.filter((d) => isWled(d))
			.sort((a, b) => (a.name_override ?? a.name).localeCompare(b.name_override ?? b.name))
	);
	// A working copy: several quick changes in a row each build on the last,
	// not on the settings the page has saved back so far.
	let draft = $state<Config>({});
	let seeded = false;
	$effect(() => {
		if (!seeded) {
			draft = { ...config };
			seeded = true;
		}
	});
	const ids = $derived(draft.device_ids ?? []);
	const first = $derived(ids.map((id) => devices.byId[id]).find((d) => !!d));
	const offered = $derived(first ? presetsOf(first).filter((p) => p.kind !== 'playlist') : []);
	const picked = $derived(draft.presets ?? []);

	function save(next: Config) {
		draft = { ...draft, ...next };
		me.dispatchEvent(new CustomEvent('change', { detail: { config: { ...draft } } }));
	}
	const flip = <T,>(list: T[], v: T) => (list.includes(v) ? list.filter((x) => x !== v) : [...list, v]);
</script>

<div class="settings">
	<label class="hc-field"
		><span class="hc-label">Title</span>
		<input
			value={draft.title ?? ''}
			placeholder="Lights"
			onchange={(e) => save({ title: e.currentTarget.value.trim() })}
		/></label
	>
	<fieldset>
		<legend class="hc-label">Controllers</legend>
		{#each all as d (d.device_id)}
			<label class="check"
				><input
					type="checkbox"
					checked={ids.includes(d.device_id)}
					onchange={() => save({ device_ids: flip(ids, d.device_id) })}
				/>{d.name_override ?? d.name}</label
			>
		{/each}
	</fieldset>
	<fieldset>
		<legend class="hc-label">Presets {first ? `(from ${first.name_override ?? first.name})` : ''}</legend>
		{#if !first}<p class="hc-quiet">Choose a controller first.</p>{/if}
		<div class="presets">
			{#each offered as p (p.id)}
				<button
					class="preset"
					aria-pressed={picked.includes(p.id)}
					onclick={() => save({ presets: flip(picked, p.id) })}
					><span class="sw" style:background={presetCss(p)}></span>{p.name}</button
				>
			{/each}
		</div>
	</fieldset>
	<p class="hc-quiet">On the other controllers, each preset is found by its name.</p>
</div>

<style>
	.settings {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	fieldset {
		border: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 34px;
	}
	.presets {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.preset {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-height: 34px;
		padding: 0 10px 0 6px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 13px;
	}
	.preset[aria-pressed='true'] {
		border-color: var(--accent);
		background: var(--lit-bg);
		font-weight: 600;
	}
	.sw {
		width: 18px;
		height: 18px;
		border-radius: 5px;
	}
</style>
