<!--
	A WLED strip's segments: the strip to scale, each segment's LEDs, how they
	run (grouping, skipping, shifting, backwards, mirrored, frozen), its own
	power, brightness and white temperature — and adding, splitting, merging
	and deleting segments.
-->
<script lang="ts">
	import type { CoreDevice } from '#lib/devices.js';
	import { commands } from '#lib/commands.svelte.js';
	import { ask } from '#lib/confirm.svelte.js';
	import { mainSegment, segmentCss, segmentsOf, type Catalog, type Segment } from '#lib/wled.js';

	let {
		device,
		catalog,
		disabled
	}: { device: CoreDevice; catalog: Catalog | undefined; disabled: boolean } = $props();

	const id = $derived(device.device_id);
	const segs = $derived(segmentsOf(device));
	const leds = $derived(Number(device.attributes.led_count ?? segs.at(-1)?.stop ?? 0));
	const max = $derived(Number(device.attributes.led_max_segments ?? 32));
	let open = $state<number | undefined>(undefined);
	const seg = $derived(segs.find((s) => s.id === open) ?? mainSegment(device));

	const one = (s: Segment, change: Record<string, unknown>) =>
		void commands.send(id, { segments: [{ id: s.id, ...change }] });
	let splitAt = $state('');

	async function split(s: Segment) {
		const at = Number(splitAt);
		if (!Number.isInteger(at) || at <= s.start || at >= s.stop) return;
		// The first part keeps the segment; the rest becomes a new one.
		await commands.send(id, { segments: [{ id: s.id, stop: at }] });
		await commands.send(id, { add_segment: { start: at, stop: s.stop } });
		splitAt = '';
	}
	async function mergeNext(s: Segment) {
		const next = segs.find((x) => x.start === s.stop);
		if (!next) return;
		await commands.send(id, { delete_segment: next.id });
		await commands.send(id, { segments: [{ id: s.id, stop: next.stop }] });
	}
	async function remove(s: Segment) {
		if (
			!(await ask({
				title: `Delete segment ${s.id}? Its LEDs go dark until another segment covers them.`,
				go: 'Delete',
				stay: 'Keep it'
			}))
		)
			return;
		void commands.send(id, { delete_segment: s.id });
		open = undefined;
	}
	function addAfter() {
		const end = segs.reduce((m, s) => Math.max(m, s.stop), 0);
		const start = end < leds ? end : Math.max(0, leds - Math.floor(leds / 4));
		void commands.send(id, { add_segment: { start, stop: leds } });
	}
	// ---- dragging a boundary ----
	type Move = { id: number; key: 'start' | 'stop' };
	type Edge = { at: number; moves: Move[]; min: number; max: number; label: string };
	const edges = $derived.by(() => {
		const sorted = [...segs].sort((a, b) => a.start - b.start);
		const out: Edge[] = [];
		sorted.forEach((s, i) => {
			const prev = sorted[i - 1];
			const next = sorted[i + 1];
			if (!prev || prev.stop !== s.start)
				out.push({
					at: s.start,
					moves: [{ id: s.id, key: 'start' }],
					min: prev ? prev.stop : 0,
					max: s.stop - 1,
					label: `Start of segment ${s.id}`
				});
			const shared = next && next.start === s.stop;
			out.push({
				at: s.stop,
				moves: shared
					? [
							{ id: s.id, key: 'stop' },
							{ id: next.id, key: 'start' }
						]
					: [{ id: s.id, key: 'stop' }],
				min: s.start + 1,
				max: shared ? next.stop - 1 : next ? next.start : leds,
				label: shared ? `Between segments ${s.id} and ${next.id}` : `End of segment ${s.id}`
			});
		});
		return out;
	});
	let drag = $state<{ edge: Edge; at: number } | null>(null);
	let mapEl = $state<HTMLDivElement | undefined>(undefined);
	/** The segments as they are, or as a drag is making them. */
	const shown = $derived(
		drag
			? segs.map((s) => {
					const m = drag!.edge.moves.filter((x) => x.id === s.id);
					return m.reduce((acc, x) => ({ ...acc, [x.key]: drag!.at }), s);
				})
			: segs
	);
	const ledAt = (clientX: number, e: Edge) => {
		const r = mapEl!.getBoundingClientRect();
		const v = Math.round(((clientX - r.left) / Math.max(1, r.width)) * leds);
		return Math.max(e.min, Math.min(e.max, v));
	};
	function commit() {
		if (!drag) return;
		const { edge, at } = drag;
		drag = null;
		if (at === edge.at) return;
		void commands.send(id, { segments: edge.moves.map((m) => ({ id: m.id, [m.key]: at })) });
	}
	function grab(e: PointerEvent, edge: Edge) {
		if (disabled) return;
		(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
		drag = { edge, at: edge.at };
	}
	function nudge(e: KeyboardEvent, edge: Edge) {
		const step = e.shiftKey ? 10 : 1;
		const d = e.key === 'ArrowLeft' ? -step : e.key === 'ArrowRight' ? step : 0;
		if (!d || disabled) return;
		e.preventDefault();
		const from = drag?.edge.label === edge.label ? drag.at : edge.at;
		drag = { edge, at: Math.max(edge.min, Math.min(edge.max, from + d)) };
	}

	const width = (s: Segment) => `${Math.max(2, ((s.stop - s.start) / Math.max(1, leds)) * 100)}%`;
	const left = (s: Segment) => `${(s.start / Math.max(1, leds)) * 100}%`;
	const BLEND = [
		'Top — covers what’s under it',
		'Add',
		'Subtract',
		'Difference',
		'Average',
		'Multiply',
		'Divide',
		'Lighten',
		'Darken',
		'Screen',
		'Overlay',
		'Hard light',
		'Soft light',
		'Dodge',
		'Burn'
	];
</script>

<div class="segs">
	<section>
		<div class="head">
			<span class="label">The strip, LED 0 to {leds - 1}</span>
			<span class="num">{segs.length} of {max}</span>
		</div>
		<div class="map" bind:this={mapEl}>
			{#each shown as s (s.id)}
				<button
					class="part"
					class:on={s.id === seg?.id}
					style:left={left(s)}
					style:width={width(s)}
					style:background={s.on
						? segmentCss(s, catalog)
						: 'repeating-linear-gradient(90deg, var(--ink-muted) 0 4px, transparent 4px 8px)'}
					tabindex="-1"
					aria-label={`Segment ${s.id}`}
					onclick={() => (open = s.id)}
				></button>
			{/each}
			{#each edges as e (e.label)}
				{@const at = drag?.edge.label === e.label ? drag.at : e.at}
				<span
					class="edge"
					class:moving={drag?.edge.label === e.label}
					style:left={`${(at / Math.max(1, leds)) * 100}%`}
					role="slider"
					tabindex={disabled ? -1 : 0}
					aria-label={e.label}
					aria-valuemin={e.min}
					aria-valuemax={e.max}
					aria-valuenow={at}
					aria-disabled={disabled}
					onpointerdown={(ev) => grab(ev, e)}
					onpointermove={(ev) =>
						drag?.edge.label === e.label && (drag = { edge: e, at: ledAt(ev.clientX, e) })}
					onpointerup={commit}
					onpointercancel={() => (drag = null)}
					onkeydown={(ev) => nudge(ev, e)}
					onkeyup={(ev) => (ev.key === 'ArrowLeft' || ev.key === 'ArrowRight') && commit()}
					onblur={commit}
				>
					{#if drag?.edge.label === e.label}<span class="at num">{at}</span>{/if}
				</span>
			{/each}
		</div>
		<div class="ends num"><span>0</span><span>{leds}</span></div>
		<p class="quiet small">Drag a boundary to move it; gaps between segments stay dark.</p>
		<div class="row-buttons">
			<button class="pill" disabled={disabled || segs.length >= max} onclick={addAfter}
				>Add a segment</button
			>
		</div>
	</section>

	<ul class="list">
		{#each segs as s (s.id)}
			<li>
				<button class="pick" class:on={s.id === seg?.id} onclick={() => (open = s.id)}>
					<span class="sw" style:background={segmentCss(s, catalog)}></span>
					<span class="what">
						<b>{s.id} · {s.name ?? 'Segment'}</b>
						<span class="num"
							>{s.start}–{s.stop - 1}{s.on ? '' : ' · off'}{s.id === device.attributes.main_segment
								? ' · main'
								: ''}</span
						>
					</span>
				</button>
			</li>
		{/each}
	</ul>

	{#if seg}
		{@const s = seg}
		<section class="card">
			<div class="title-row">
				<input
					class="name"
					aria-label="Segment name"
					value={s.name ?? ''}
					placeholder={`Segment ${s.id}`}
					{disabled}
					onchange={(e) => one(s, { name: e.currentTarget.value })}
				/>
				<label class="check">
					<input
						type="checkbox"
						checked={s.on}
						{disabled}
						onchange={(e) => one(s, { segment_on: e.currentTarget.checked })}
					/>
					On
				</label>
			</div>
			<div class="grid2">
				<label>
					<span class="label"
						>Brightness · <span class="num">{Math.round(s.brightness_pct)}%</span></span
					>
					<input
						type="range"
						min="0"
						max="100"
						value={s.brightness_pct}
						{disabled}
						onchange={(e) => one(s, { segment_brightness_pct: Number(e.currentTarget.value) })}
					/>
				</label>
				{#if device.attributes.led_cct === true}
					<label>
						<span class="label">White temperature · <span class="num">{s.cct}</span></span>
						<input
							class="warm"
							type="range"
							min="0"
							max="255"
							value={s.cct}
							{disabled}
							onchange={(e) => one(s, { cct: Number(e.currentTarget.value) })}
						/>
					</label>
				{/if}
			</div>
			<div class="span">
				<span class="label">Which LEDs</span>
				<div class="grid5">
					{#each [['From LED', 'start', s.start], ['To LED', 'stop', s.stop], ['Group', 'grouping', s.grouping], ['Skip', 'spacing', s.spacing], ['Shift', 'offset', s.offset]] as [label, key, v] (key)}
						<label>
							<span class="label">{label}</span>
							<input
								class="n"
								type="number"
								min="0"
								value={key === 'stop' ? Number(v) - 1 : v}
								{disabled}
								onchange={(e) =>
									one(s, {
										[key as string]:
											key === 'stop'
												? Number(e.currentTarget.value) + 1
												: Number(e.currentTarget.value)
									})}
							/>
						</label>
					{/each}
				</div>
				<p class="quiet small">
					Group lights LEDs in runs as one; Skip leaves LEDs dark between runs; Shift moves where
					the effect starts.
				</p>
			</div>
			<div class="flags">
				{#each [['Run backwards', 'reverse', s.reverse], ['Mirror from the middle', 'mirror', s.mirror], ['Freeze the effect', 'freeze', s.freeze], ['Selected', 'selected', s.selected]] as [label, key, v] (key)}
					<label class="check">
						<input
							type="checkbox"
							checked={v as boolean}
							{disabled}
							onchange={(e) => one(s, { [key as string]: e.currentTarget.checked })}
						/>
						{label}
					</label>
				{/each}
			</div>
			<label>
				<span class="label">Blend with the segments below</span>
				<select
					value={s.blend_mode}
					{disabled}
					onchange={(e) => one(s, { blend_mode: Number(e.currentTarget.value) })}
				>
					{#each BLEND as b, i (b)}<option value={i}>{b}</option>{/each}
				</select>
			</label>
			<div class="row-buttons">
				{#if s.id !== device.attributes.main_segment}
					<button class="pill" {disabled} onclick={() => commands.send(id, { main_segment: s.id })}
						>Make it the main segment</button
					>
				{/if}
				<form class="inline" onsubmit={(e) => (e.preventDefault(), split(s))}>
					<input
						class="n"
						type="number"
						placeholder="at LED"
						aria-label="Split at LED"
						bind:value={splitAt}
						min={s.start + 1}
						max={s.stop - 1}
					/>
					<button class="pill" disabled={disabled || segs.length >= max || !splitAt}>Split</button>
				</form>
				{#if segs.some((x) => x.start === s.stop)}
					<button class="pill" {disabled} onclick={() => mergeNext(s)}>Merge with the next</button>
				{/if}
				{#if segs.length > 1}
					<button class="pill danger" {disabled} onclick={() => remove(s)}>Delete segment</button>
				{/if}
			</div>
		</section>
	{/if}
</div>

<style>
	.segs {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	section {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.card {
		gap: 14px;
		padding: 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius);
		background: var(--surface);
	}
	.head,
	.ends {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}
	.label {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
	}
	.num {
		font-family: var(--font-num);
		font-size: 13px;
		color: var(--ink-muted);
	}
	.small {
		font-size: 13px;
		margin: 0;
	}
	.quiet {
		color: var(--ink-muted);
	}
	.map {
		position: relative;
		height: 40px;
		border-radius: 8px;
		border: 1px solid var(--line);
		background: var(--surface-2);
		overflow: hidden;
	}
	.part {
		position: absolute;
		top: 0;
		bottom: 0;
		border: none;
		border-right: 2px solid var(--sheet);
		padding: 0;
	}
	.edge {
		user-select: none;
		position: absolute;
		top: 0;
		bottom: 0;
		width: 16px;
		margin-left: -8px;
		cursor: ew-resize;
		touch-action: none;
		z-index: 1;
	}
	.edge::after {
		content: '';
		position: absolute;
		left: 6px;
		top: 6px;
		bottom: 6px;
		width: 4px;
		border-radius: 2px;
		background: var(--sheet);
		box-shadow: 0 0 0 1px var(--ink-muted);
	}
	.edge.moving::after,
	.edge:focus-visible::after {
		background: var(--accent);
	}
	.edge[aria-disabled='true'] {
		cursor: default;
	}
	.at {
		position: absolute;
		top: 2px;
		left: 14px;
		padding: 0 4px;
		border-radius: 4px;
		background: var(--sheet);
		color: var(--ink);
		font-size: 11px;
	}
	.part.on {
		outline: 3px solid var(--accent);
		outline-offset: -3px;
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.pick {
		font: inherit;
		display: flex;
		gap: 10px;
		align-items: center;
		padding: 8px 12px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		text-align: left;
	}
	.pick.on {
		border-color: var(--accent);
		background: var(--lit-bg);
	}
	.sw {
		width: 34px;
		height: 18px;
		border-radius: 4px;
		flex: none;
	}
	.what {
		display: flex;
		flex-direction: column;
	}
	.title-row {
		display: flex;
		gap: 12px;
		align-items: center;
		flex-wrap: wrap;
	}
	.name {
		flex: 1 1 200px;
		font: inherit;
		font-size: 18px;
		font-weight: 700;
		min-height: 40px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 10px;
	}
	.grid2 {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 12px 20px;
	}
	.grid5 {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(80px, 1fr));
		gap: 10px;
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.n,
	select {
		font: inherit;
		font-family: var(--font-num);
		min-height: 38px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 8px;
		min-width: 0;
	}
	select {
		font-family: inherit;
	}
	input[type='range'] {
		width: 100%;
		accent-color: var(--track-on);
	}
	input.warm {
		accent-color: #e0b070;
	}
	.flags {
		display: flex;
		flex-wrap: wrap;
		gap: 0 22px;
	}
	.check {
		flex-direction: row;
		align-items: center;
		gap: 8px;
		min-height: var(--target);
	}
	.check input {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.row-buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.inline {
		display: flex;
		gap: 6px;
	}
	.inline .n {
		width: 90px;
	}
	.pill {
		font: inherit;
		font-size: 14px;
		min-height: 40px;
		padding: 0 14px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.pill.danger {
		color: var(--alert);
		border-color: var(--alert-line);
	}
	button:disabled {
		opacity: 0.55;
	}
</style>
