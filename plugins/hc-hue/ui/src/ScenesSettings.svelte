<svelte:options
	customElement={{
		tag: 'hc-hue-scenes-settings',
		props: { host: { type: 'Object' }, config: { type: 'Object' } }
	}}
/>

<!-- "Hue scenes" settings: what it is called, which rooms, which of their
     scenes, and whether it has an All off. -->
<script>
	import { adopt, groupsOf, paletteOf, scenesOf } from './lib/hue.js';

	let { host: hc, config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const groups = $derived(groupsOf(devices));

	// A working copy: the config handed in lags a moment behind each save,
	// so quick changes build on this, not on it.
	let draft = $state(null);
	const cfg = $derived(draft ?? { name: '', rooms: [], scenes: [], alloff: true, ...config });
	function save(next) {
		draft = next;
		me.dispatchEvent(new CustomEvent('change', { detail: { config: next } }));
	}
	const rooms = $derived(cfg.rooms ?? []);
	const toggleRoom = (rid) =>
		save({ ...cfg, rooms: rooms.includes(rid) ? rooms.filter((r) => r !== rid) : [...rooms, rid] });
	const offered = $derived(rooms.flatMap((rid) => scenesOf(devices, rid)));
	const picked = $derived(cfg.scenes ?? []);
	function toggleScene(id) {
		const all = picked.length ? picked : offered.map((s) => s.device_id);
		const next = all.includes(id) ? all.filter((x) => x !== id) : [...all, id];
		save({ ...cfg, scenes: next.length === offered.length ? [] : next });
	}
	const isPicked = (id) => !picked.length || picked.includes(id);
</script>

<div class="settings">
	<label class="hc-field"
		><span class="hc-label">Name</span>
		<input value={cfg.name ?? ''} placeholder="Hue scenes" onchange={(e) => save({ ...cfg, name: e.currentTarget.value })} /></label
	>
	<span class="hc-label">Rooms and zones</span>
	<span class="hc-row">
		{#each groups as g (g.rid)}
			<button class="hc-pill" class:go={rooms.includes(g.rid)} aria-pressed={rooms.includes(g.rid)} onclick={() => toggleRoom(g.rid)}
				>{g.name}{g.kind === 'zone' ? ' (zone)' : ''}</button
			>
		{/each}
	</span>
	{#if offered.length}
		<span class="hc-label">Scenes — all of them, or the ones you tick</span>
		<div class="list">
			{#each offered as s (s.device_id)}
				<label class="check">
					<input type="checkbox" checked={isPicked(s.device_id)} onchange={() => toggleScene(s.device_id)} />
					<span class="pal">{#each paletteOf(s) as c, i (i)}<span style:background={c}></span>{/each}</span>
					{hc.devices.label(s)}{rooms.length > 1 ? ` · ${s.attributes.group_name}` : ''}
				</label>
			{/each}
		</div>
	{/if}
	<label class="check"
		><input type="checkbox" checked={cfg.alloff !== false} onchange={(e) => save({ ...cfg, alloff: e.currentTarget.checked })} />Show an “All off” button</label
	>
</div>

<style>
	.settings {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.list {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
		gap: 6px 14px;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 14px;
	}
	.pal {
		display: inline-flex;
		width: 34px;
		height: 14px;
		border-radius: 4px;
		overflow: hidden;
		flex: none;
	}
	.pal span {
		flex: 1;
	}
</style>
