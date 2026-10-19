<svelte:options
	customElement={{
		tag: 'hc-hue-scenes',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Hue scenes", a card you add: the scenes of the rooms you chose, to
     tap, and all their lights off. Set up in its settings. -->
<script>
	import Scenes from './lib/SceneTiles.svelte';
	import { adopt, groupsOf, runningIn, scenesOf } from './lib/hue.js';

	let { host: hc, form = 'large', config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));

	const groups = $derived(groupsOf(devices));
	const chosen = $derived(
		Array.isArray(config?.rooms) && config.rooms.length ? groups.filter((g) => config.rooms.includes(g.rid)) : groups.slice(0, 1)
	);
	let only = $state(null);
	const showing = $derived(only ? chosen.filter((g) => g.rid === only) : chosen);
	const picked = $derived(Array.isArray(config?.scenes) && config.scenes.length ? config.scenes : null);
	const scenes = $derived(
		showing.flatMap((g) => scenesOf(devices, g.rid)).filter((s) => !picked || picked.includes(s.device_id))
	);
	const running = $derived(
		chosen
			.map((g) => {
				const s = runningIn(devices, g.rid);
				return s ? `${hc.devices.label(s)} in ${g.name}` : '';
			})
			.filter(Boolean)
			.join(' · ')
	);
	const title = $derived(config?.name || (chosen.length === 1 ? `${chosen[0].name} scenes` : 'Hue scenes'));

	let error = $state('');
	async function allOff() {
		const rids = new Set(chosen.map((g) => g.rid));
		const lights = devices.filter((d) => d.device_type === 'light' && rids.has(d.attributes.room_rid) && d.attributes.on);
		const rooms = devices.filter((d) => d.device_type === 'group' && rids.has(d.attributes.group_rid));
		const targets = rooms.length === chosen.length ? rooms : lights;
		const results = await Promise.all(targets.map((d) => hc.command(d.device_id, { on: false })));
		const bad = results.find((r) => !r.ok);
		error = bad ? bad.error : '';
	}
	const cols = $derived(form === 'large' ? 4 : 2);
</script>

<div class="hc-tile card {form}">
	<div class="top">
		<span class="words">
			<span class="hc-tile-name">{title}</span>
			<span class="hc-tile-state">{running || 'No scene running'}</span>
		</span>
		{#if chosen.length > 1 && form === 'large'}
			<span class="hc-row">
				{#each chosen as g (g.rid)}
					<button class="hc-pill chip" class:go={only === g.rid} onclick={() => (only = only === g.rid ? null : g.rid)}>{g.name}</button>
				{/each}
			</span>
		{/if}
	</div>
	{#if scenes.length}
		<Scenes {hc} scenes={form === 'large' ? scenes.slice(0, 12) : scenes.slice(0, 4)} columns={cols} compact={form !== 'large'} where={chosen.length > 1 && !only} onerror={(e) => (error = e)} />
	{:else}
		<p class="hc-quiet">No scenes yet — choose rooms in this card's settings.</p>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}
	{#if config?.alloff !== false}
		<span class="end"><button class="hc-pill chip" onclick={allOff}>All off</button></span>
	{/if}
</div>

<style>
	.card {
		height: 100%;
		gap: 12px;
	}
	.top {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 10px;
		flex-wrap: wrap;
	}
	.words {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.chip {
		min-height: 32px;
		font-size: 13px;
	}
	.end {
		display: flex;
		justify-content: flex-end;
		margin-top: auto;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 13px;
	}
</style>
