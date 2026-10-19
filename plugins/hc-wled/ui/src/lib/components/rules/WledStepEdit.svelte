<!--
	A rule's step for WLED controllers, in WLED's terms: apply a preset (by
	name), start a playlist, set a look (effect, palette, the effect's own
	controls, colours — for every segment or some), the nightlight, power,
	sync and live data. On one controller it is a "set a device" step; on
	several, steps that run together, one per controller, a preset matched by
	name on each. Read back from what it stores, so a step written by hand
	opens here too.
-->
<script lang="ts">
	import { displayName, type CoreDevice } from '#lib/devices.js';
	import { wledCatalogs } from '#lib/wledCatalog.svelte.js';
	import {
		effectNames,
		hex,
		isWled,
		paletteNames,
		playlistsOf,
		presetCss,
		presetOn,
		presetsOf,
		segmentsOf,
		stepSentence,
		stepState,
		type StepKind,
		type WledStep
	} from '#lib/wled.js';
	import { editor } from './context.js';

	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	type Json = any;
	let {
		ids,
		step,
		onchange
	}: {
		/** The controllers, the first is the one the step was made on. */
		ids: string[];
		step: WledStep;
		/** The step as one action: a SetDeviceState, or Parallel for several. */
		onchange: (action: Json) => void;
	} = $props();

	const ctx = editor();
	const all = $derived(ctx.devices.filter((d) => isWled(d)));
	const first = $derived(ctx.house.device(ids[0] ?? '') as CoreDevice | undefined);
	const chosen = $derived(
		ids.map((id) => ctx.house.device(id)).filter((d): d is CoreDevice => !!d)
	);
	$effect(() => {
		if (first) wledCatalogs.ensure(first);
	});
	const catalog = $derived(first ? wledCatalogs.of(first).catalog : undefined);

	/** The step's action, for these controllers. */
	function actionFor(nextIds: string[], s: WledStep): Json {
		const from = ctx.house.device(nextIds[0] ?? '');
		const per = nextIds
			.map((id) => {
				const d = ctx.house.device(id);
				if (!d || !from) return null;
				// A preset is the same preset by name, wherever it's kept.
				if (s.kind === 'preset' && id !== nextIds[0]) {
					const p = presetOn(d, from, s.preset_id ?? 0);
					return p === undefined ? null : { id, state: stepState({ ...s, preset_id: p }) };
				}
				if (s.kind === 'playlist' && id !== nextIds[0]) {
					const p = presetOn(d, from, s.playlist_id ?? 0);
					return p === undefined ? null : { id, state: stepState({ ...s, playlist_id: p }) };
				}
				return { id, state: stepState(s) };
			})
			.filter((x): x is { id: string; state: Json } => x !== null);
		if (per.length <= 1) {
			const one = per[0] ?? { id: nextIds[0] ?? '', state: stepState(s) };
			return { SetDeviceState: { device_id: one.id, state: one.state, track_event_value: false } };
		}
		return {
			Parallel: {
				actions: per.map((p) => ({
					SetDeviceState: { device_id: p.id, state: p.state, track_event_value: false }
				}))
			}
		};
	}
	const save = (s: WledStep, nextIds = ids) => onchange(actionFor(nextIds, s));
	const patch = (change: Partial<WledStep>) => save({ ...step, ...change });

	function toggle(id: string) {
		const next = ids.includes(id) ? ids.filter((x) => x !== id) : [...ids, id];
		if (next.length) save(step, next);
	}
	/** Controllers that would be left out: no preset of that name. */
	const missing = $derived(
		step.kind === 'preset' || step.kind === 'playlist'
			? chosen
					.slice(1)
					.filter(
						(d) =>
							first &&
							presetOn(
								d,
								first,
								(step.kind === 'preset' ? step.preset_id : step.playlist_id) ?? 0
							) === undefined
					)
			: []
	);
	const presetName = (id?: number) =>
		first ? presetsOf(first).find((p) => p.id === id)?.name : undefined;

	const MODES: [StepKind, string][] = [
		['preset', 'Apply a preset'],
		['playlist', 'Start a playlist'],
		['look', 'Set a look'],
		['nightlight', 'Nightlight'],
		['power', 'Turn on or off'],
		['sync', 'Sync and live data']
	];
	function kindTo(k: StepKind) {
		const presets = first ? presetsOf(first) : [];
		switch (k) {
			case 'preset':
				return save({
					kind: 'preset',
					preset_id: presets.find((p) => p.kind !== 'playlist')?.id ?? 1
				});
			case 'playlist':
				return save({
					kind: 'playlist',
					playlist_id: (first ? playlistsOf(first)[0]?.id : undefined) ?? 1
				});
			case 'look':
				return save({ kind: 'look', segment: 'all', look: { effect_id: 0 } });
			case 'nightlight':
				return save({
					kind: 'nightlight',
					nightlight: { on: true, duration_min: 30, mode: 'fade', target_pct: 0 }
				});
			case 'power':
				return save({ kind: 'power', power: true });
			case 'sync':
				return save({ kind: 'sync', sync: { receive: true } });
		}
	}

	// ---- a look ----
	const look = $derived(step.look ?? {});
	const fx = $derived(catalog?.effects.find((e) => e.id === look.effect_id));
	const sliders = $derived(
		fx?.sliders ?? [
			{ key: 'effect_speed', label: 'Speed', max: 255 },
			{ key: 'effect_intensity', label: 'Intensity', max: 255 }
		]
	);
	const setLook = (k: string, v: unknown) => {
		const next = { ...look };
		if (v === undefined) delete next[k];
		else next[k] = v;
		patch({ look: next });
	};
	const COLOUR_KEY = ['color', 'color2', 'color3'];
	const segs = $derived(first ? segmentsOf(first) : []);
	const effects = $derived(
		catalog?.effects.map((e) => ({ id: e.id, name: e.name })) ?? (first ? effectNames(first) : [])
	);
	const palettes = $derived(
		catalog?.palettes.map((p) => ({ id: p.id, name: p.name })) ?? (first ? paletteNames(first) : [])
	);

	const sentence = $derived(
		stepSentence(step, chosen.map((d) => displayName(d)).join(' and ') || '…', {
			preset: presetName,
			effect: (id) => effects.find((e) => e.id === id)?.name,
			palette: (id) => palettes.find((p) => p.id === id)?.name
		})
	);
</script>

<div class="wled-step">
	<p class="say">{sentence}</p>

	<div class="row">
		<span class="label">On</span>
		<div class="chips">
			{#each all as d (d.device_id)}
				<button
					type="button"
					aria-pressed={ids.includes(d.device_id)}
					onclick={() => toggle(d.device_id)}
					>{ids.includes(d.device_id) ? '✓ ' : '+ '}{displayName(d)}</button
				>
			{/each}
		</div>
	</div>
	{#if missing.length}
		<p class="warn">
			{missing.map((d) => displayName(d)).join(', ')}
			{missing.length === 1 ? 'has' : 'have'} no “{presetName(
				step.kind === 'preset' ? step.preset_id : step.playlist_id
			)}”, so {missing.length === 1 ? 'it is' : 'they are'} left out.
		</p>
	{/if}

	<div class="row">
		<span class="label">Do</span>
		<div class="chips">
			{#each MODES as [k, label] (k)}
				<button type="button" aria-pressed={step.kind === k} onclick={() => kindTo(k)}
					>{label}</button
				>
			{/each}
		</div>
	</div>

	{#if step.kind === 'preset' && first}
		<div class="presets">
			{#each presetsOf(first).filter((p) => p.kind !== 'playlist') as p (p.id)}
				<button
					type="button"
					class="preset"
					class:on={p.id === step.preset_id}
					onclick={() => patch({ preset_id: p.id })}
				>
					<span class="sw" style:background={presetCss(p)}></span>
					<span>{p.name}</span>
				</button>
			{/each}
		</div>
	{:else if step.kind === 'playlist' && first}
		<label class="field">
			<span class="label">Playlist</span>
			<select
				value={step.playlist_id}
				onchange={(e) => patch({ playlist_id: Number(e.currentTarget.value) })}
			>
				{#each playlistsOf(first) as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
				{#if !playlistsOf(first).some((p) => p.id === step.playlist_id)}<option
						value={step.playlist_id}>Slot {step.playlist_id}</option
					>{/if}
			</select>
		</label>
	{:else if step.kind === 'look'}
		<div class="grid">
			<label class="field">
				<span class="label">Segments</span>
				<select
					value={String(step.segment ?? 'all')}
					onchange={(e) => {
						const v = e.currentTarget.value;
						patch({ segment: v === 'all' || v === 'selected' ? v : Number(v) });
					}}
				>
					<option value="all">Every segment</option>
					<option value="selected">The selected ones</option>
					{#each segs as s (s.id)}<option value={String(s.id)}
							>{s.id} · {s.name ?? `LEDs ${s.start}–${s.stop - 1}`}</option
						>{/each}
				</select>
			</label>
			<label class="field">
				<span class="label">Effect</span>
				<select
					value={look.effect_id ?? ''}
					onchange={(e) =>
						setLook(
							'effect_id',
							e.currentTarget.value === '' ? undefined : Number(e.currentTarget.value)
						)}
				>
					<option value="">Leave it as it is</option>
					{#each effects as e (e.id)}<option value={e.id}>{e.name}</option>{/each}
				</select>
			</label>
			{#if !fx || fx.palette !== null}
				<label class="field">
					<span class="label">Palette</span>
					<select
						value={look.palette_id ?? ''}
						onchange={(e) =>
							setLook(
								'palette_id',
								e.currentTarget.value === '' ? undefined : Number(e.currentTarget.value)
							)}
					>
						<option value="">Leave it as it is</option>
						{#each palettes as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
					</select>
				</label>
			{/if}
			{#each sliders as s (s.key)}
				<label class="field">
					<span class="label">{s.label} · <span class="num">{look[s.key] ?? 'as it is'}</span></span
					>
					<input
						type="range"
						min="0"
						max={s.max}
						value={Number(look[s.key] ?? Math.round(s.max / 2))}
						onchange={(e) => setLook(s.key, Number(e.currentTarget.value))}
					/>
				</label>
			{/each}
		</div>
		{#if fx?.options.length}
			<div class="chips">
				{#each fx.options as o (o.key)}
					<label class="check"
						><input
							type="checkbox"
							checked={look[o.key] === true}
							onchange={(e) => setLook(o.key, e.currentTarget.checked)}
						/>
						{o.label}</label
					>
				{/each}
			</div>
		{/if}
		<div class="colours">
			<span class="label">Colours</span>
			{#each fx?.colors ?? [{ slot: 0, label: 'Primary' }, { slot: 1, label: 'Background' }, { slot: 2, label: 'Third' }] as c (c.slot)}
				{@const k = COLOUR_KEY[c.slot]!}
				<label class="check">
					<input
						type="checkbox"
						checked={look[k] !== undefined}
						onchange={(e) =>
							setLook(k, e.currentTarget.checked ? (look[k] ?? [255, 160, 0]) : undefined)}
					/>
					{c.label}
					{#if look[k] !== undefined}
						<input
							type="color"
							value={hex(look[k])}
							onchange={(e) => setLook(k, e.currentTarget.value)}
						/>
					{/if}
				</label>
			{/each}
		</div>
		<p class="quiet">Only what's set changes; the rest stays as the strip has it.</p>
	{:else if step.kind === 'nightlight'}
		{@const n = step.nightlight ?? { on: true, duration_min: 30, mode: 'fade', target_pct: 0 }}
		<div class="grid">
			<label class="field"
				><span class="label">Nightlight</span>
				<select
					value={n.on ? 'on' : 'off'}
					onchange={(e) => patch({ nightlight: { ...n, on: e.currentTarget.value === 'on' } })}
				>
					<option value="on">Start it</option><option value="off">Stop it</option>
				</select></label
			>
			{#if n.on}
				<label class="field"
					><span class="label">For (min)</span>
					<input
						type="number"
						min="1"
						max="255"
						value={n.duration_min}
						onchange={(e) =>
							patch({ nightlight: { ...n, duration_min: Number(e.currentTarget.value) } })}
					/></label
				>
				<label class="field"
					><span class="label">Down to (%)</span>
					<input
						type="number"
						min="0"
						max="100"
						value={n.target_pct}
						onchange={(e) =>
							patch({ nightlight: { ...n, target_pct: Number(e.currentTarget.value) } })}
					/></label
				>
				<label class="field"
					><span class="label">How</span>
					<select
						value={n.mode}
						onchange={(e) => patch({ nightlight: { ...n, mode: e.currentTarget.value } })}
					>
						<option value="instant">All at once</option><option value="fade">Fade</option><option
							value="color">Fade the colour</option
						><option value="sunrise">Sunrise</option>
					</select></label
				>
			{/if}
		</div>
	{:else if step.kind === 'power'}
		<div class="chips">
			{#each [[true, 'Turn on'], [false, 'Turn off'], ['toggle', 'Flip']] as [v, label] (String(v))}
				<button
					type="button"
					aria-pressed={step.power === v}
					onclick={() => patch({ power: v as boolean | 'toggle' })}>{label}</button
				>
			{/each}
		</div>
	{:else if step.kind === 'sync'}
		{@const sy = step.sync ?? {}}
		<div class="grid">
			{#each [['send', 'Send changes to other WLEDs'], ['receive', 'Follow other WLEDs']] as [k, label] (k)}
				<label class="field"
					><span class="label">{label}</span>
					<select
						value={sy[k as 'send' | 'receive'] === undefined
							? ''
							: String(sy[k as 'send' | 'receive'])}
						onchange={(e) => {
							const v = e.currentTarget.value;
							patch({ sync: { ...sy, [k as string]: v === '' ? undefined : v === 'true' } });
						}}
					>
						<option value="">Leave it</option><option value="true">Yes</option><option value="false"
							>No</option
						>
					</select></label
				>
			{/each}
			<label class="field"
				><span class="label">While live data streams</span>
				<select
					value={sy.live_override === undefined ? '' : String(sy.live_override)}
					onchange={(e) => {
						const v = e.currentTarget.value;
						patch({ sync: { ...sy, live_override: v === '' ? undefined : Number(v) } });
					}}
				>
					<option value="">Leave it</option><option value="0">Let it take over</option><option
						value="1">Ignore it until it stops</option
					><option value="2">Ignore it until restart</option>
				</select></label
			>
		</div>
	{/if}

	{#if step.kind === 'preset' || step.kind === 'playlist' || step.kind === 'look'}
		<div class="grid">
			<label class="field"
				><span class="label">Brightness</span>
				<select
					value={step.brightness_pct === undefined ? '' : 'set'}
					onchange={(e) =>
						patch({
							brightness_pct: e.currentTarget.value === '' ? undefined : (step.brightness_pct ?? 50)
						})}
				>
					<option value=""
						>{step.kind === 'look' ? 'Leave it as it is' : 'As the preset has it'}</option
					>
					<option value="set">Set it to…</option>
				</select></label
			>
			{#if step.brightness_pct !== undefined}
				<label class="field"
					><span class="label">Brightness · <span class="num">{step.brightness_pct}%</span></span>
					<input
						type="range"
						min="1"
						max="100"
						value={step.brightness_pct}
						onchange={(e) => patch({ brightness_pct: Number(e.currentTarget.value) })}
					/></label
				>
			{/if}
			<label class="field"
				><span class="label">Fade over</span>
				<select
					value={step.transition === undefined ? '' : String(step.transition)}
					onchange={(e) =>
						patch({
							transition: e.currentTarget.value === '' ? undefined : Number(e.currentTarget.value)
						})}
				>
					<option value="">The controller's own</option>
					<option value="0">Instantly</option>
					{#each [700, 2000, 5000, 10000, 30000] as ms (ms)}<option value={String(ms)}
							>{ms / 1000} s</option
						>{/each}
					{#if step.transition !== undefined && ![0, 700, 2000, 5000, 10000, 30000].includes(step.transition)}<option
							value={String(step.transition)}>{step.transition / 1000} s</option
						>{/if}
				</select></label
			>
		</div>
	{/if}

	<details class="stored">
		<summary>What homeCore stores</summary>
		<pre>{JSON.stringify(stepState(step), null, 2)}</pre>
	</details>
</div>

<style>
	.wled-step {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.say {
		margin: 0;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--sheet);
		border: 1px solid var(--card-line);
		font-size: 15px;
	}
	.row {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.label {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
	}
	.num {
		font-family: var(--font-num);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.chips button {
		font: inherit;
		font-size: 14px;
		min-height: 38px;
		padding: 0 12px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.chips button[aria-pressed='true'] {
		border-color: var(--accent);
		background: var(--lit-bg);
		color: var(--lit-ink);
		font-weight: 600;
	}
	.warn {
		margin: 0;
		font-size: 13px;
		color: var(--alert);
	}
	.presets {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(130px, 1fr));
		gap: 8px;
	}
	.preset {
		font: inherit;
		font-size: 14px;
		text-align: left;
		padding: 8px;
		border-radius: 11px;
		border: 2px solid var(--card-line);
		background: var(--surface);
		color: var(--ink);
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.preset.on {
		border-color: var(--accent);
	}
	.sw {
		display: block;
		height: 14px;
		border-radius: 4px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
		gap: 10px 16px;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}
	select,
	input[type='number'] {
		height: 40px;
		padding: 0 8px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font: inherit;
		font-size: 15px;
		min-width: 0;
	}
	input[type='range'] {
		accent-color: var(--track-on);
	}
	.check {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 38px;
		font-size: 14px;
	}
	.check input[type='checkbox'] {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.check input[type='color'] {
		width: 40px;
		height: 30px;
		border: none;
		background: none;
		padding: 0;
	}
	.colours {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 18px;
		align-items: center;
	}
	.quiet {
		margin: 0;
		font-size: 13px;
		color: var(--ink-muted);
	}
	.stored summary {
		cursor: pointer;
		font-size: 13px;
		color: var(--ink-muted);
	}
	.stored pre {
		margin: 6px 0 0;
		padding: 10px;
		border-radius: 9px;
		background: var(--sheet);
		font-family: var(--font-num);
		font-size: 12px;
		white-space: pre-wrap;
	}
</style>
