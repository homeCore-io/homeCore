<svelte:options
	customElement={{
		tag: 'hc-caseta-scenes-settings',
		props: { host: { type: 'Object' }, config: { type: 'Object' } }
	}}
/>

<!-- "Caséta scenes" settings: what it is called, and which scenes, in what
     order — ticked, and dragged (or moved with the arrow keys) into place. -->
<script>
	import { adopt } from './lib/lutron.js';

	let { host: hc, config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const all = $derived(
		devices
			.filter((d) => d.device_type === 'scene')
			.sort((x, y) => hc.devices.label(x).localeCompare(hc.devices.label(y)))
	);

	// A working copy: the config handed in lags a moment behind each save.
	let draft = $state(null);
	const cfg = $derived(draft ?? { name: '', ...config });
	function save(next) {
		draft = next;
		me.dispatchEvent(new CustomEvent('change', { detail: { config: next } }));
	}
	// Ticked, in their order, then the rest as the page lists them. With
	// nothing chosen yet the card shows the first six, so they start ticked.
	const picked = $derived(
		Array.isArray(cfg.scenes) && cfg.scenes.length
			? cfg.scenes.filter((id) => all.some((s) => s.device_id === id))
			: all.slice(0, 6).map((s) => s.device_id)
	);
	const rows = $derived([
		...picked.map((id) => all.find((s) => s.device_id === id)),
		...all.filter((s) => !picked.includes(s.device_id))
	]);
	const toggle = (id) =>
		save({
			...cfg,
			scenes: picked.includes(id) ? picked.filter((x) => x !== id) : [...picked, id]
		});
	function move(id, to) {
		const order = rows.map((s) => s.device_id);
		const from = order.indexOf(id);
		if (from < 0 || to < 0 || to >= order.length || from === to) return;
		order.splice(to, 0, ...order.splice(from, 1));
		save({ ...cfg, scenes: order.filter((x) => picked.includes(x)) });
	}

	let dragging = $state(null);
	let over = $state(null);
	function keys(e, id, i) {
		if (e.key === 'ArrowUp') move(id, i - 1);
		else if (e.key === 'ArrowDown') move(id, i + 1);
		else return;
		e.preventDefault();
		queueMicrotask(() =>
			me.shadowRoot?.querySelector(`[data-id="${CSS.escape(id)}"] .grip`)?.focus()
		);
	}
</script>

<div class="settings">
	<label class="hc-field"
		><span class="hc-label">Name</span>
		<input
			value={cfg.name ?? ''}
			placeholder="Caséta scenes"
			onchange={(e) => save({ ...cfg, name: e.currentTarget.value })}
		/></label
	>
	<div class="box">
		<span class="title">Which scenes</span>
		<ul>
			{#each rows as s, i (s.device_id)}
				<li
					data-id={s.device_id}
					class:over={over === s.device_id && dragging !== s.device_id}
					draggable="true"
					ondragstart={(e) => {
						dragging = s.device_id;
						e.dataTransfer.effectAllowed = 'move';
						e.dataTransfer.setData('text/plain', s.device_id);
					}}
					ondragover={(e) => {
						e.preventDefault();
						over = s.device_id;
					}}
					ondragleave={() => over === s.device_id && (over = null)}
					ondrop={(e) => {
						e.preventDefault();
						if (dragging) move(dragging, i);
						dragging = null;
						over = null;
					}}
					ondragend={() => {
						dragging = null;
						over = null;
					}}
				>
					<label class="check">
						<input
							type="checkbox"
							checked={picked.includes(s.device_id)}
							onchange={() => toggle(s.device_id)}
						/>
						<span class="name">{hc.devices.label(s)}</span>
					</label>
					<button
						class="grip"
						aria-label={`Move ${hc.devices.label(s)} — arrow keys`}
						onkeydown={(e) => keys(e, s.device_id, i)}>⋮⋮</button
					>
				</li>
			{/each}
		</ul>
		<span class="hc-quiet">Drag to order. The card shows them in this order.</span>
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
		gap: 10px;
		padding: 16px;
		border-radius: var(--radius, 14px);
		border: 1px solid var(--card-line);
		background: var(--surface);
	}
	.title {
		font-weight: 600;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
	}
	li {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 36px;
		border-radius: 8px;
	}
	li.over {
		box-shadow: inset 0 2px 0 var(--accent);
	}
	.check {
		flex: 1;
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 14px;
		min-width: 0;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.grip {
		border: none;
		background: none;
		color: color-mix(in srgb, var(--ink-muted) 75%, var(--surface));
		cursor: grab;
		padding: 4px 6px;
		letter-spacing: -2px;
	}
	.hc-quiet {
		font-size: 12px;
	}
</style>
