<!--
	A WLED segment's look: which segments a change goes to, the effect (found
	by name, with what it uses), only the controls that effect uses — named as
	the controller names them — its colours and palette, and saving the look as
	a preset.
-->
<script lang="ts">
	import type { CoreDevice } from '#lib/devices.js';
	import { commands } from '#lib/commands.svelte.js';
	import {
		effectCommand,
		findEffects,
		lookCommand,
		mainSegment,
		paletteCss,
		presetsOf,
		rgbOf,
		segmentsOf,
		type Catalog,
		type Effect,
		type EffectFilter,
		type SegmentTarget
	} from '#lib/wled.js';

	let {
		device,
		catalog,
		disabled
	}: { device: CoreDevice; catalog: Catalog | undefined; disabled: boolean } = $props();

	const id = $derived(device.device_id);
	const segs = $derived(segmentsOf(device));
	let target = $state<SegmentTarget | undefined>(undefined);
	const chosen = $derived<SegmentTarget>(target ?? mainSegment(device)?.id ?? 0);
	const seg = $derived(
		typeof chosen === 'number'
			? (segs.find((s) => s.id === chosen) ?? segs[0])
			: mainSegment(device)
	);

	// ---- favourites, kept on this screen only ----
	const FAV_KEY = $derived(`wled-favourites:${id}`);
	let favourites = $state<Set<number>>(new Set());
	$effect(() => {
		try {
			favourites = new Set(JSON.parse(localStorage.getItem(FAV_KEY) ?? '[]') as number[]);
		} catch {
			favourites = new Set();
		}
	});
	function toggleFavourite(e: Effect) {
		const next = new Set(favourites);
		if (next.has(e.id)) next.delete(e.id);
		else next.add(e.id);
		favourites = next;
		try {
			localStorage.setItem(FAV_KEY, JSON.stringify([...next]));
		} catch {
			// A private window: favourites last until it closes.
		}
	}

	let q = $state('');
	let filter = $state<EffectFilter>('all');
	const effects = $derived(catalog?.effects ?? []);
	const found = $derived(findEffects(effects, q, filter, favourites));
	const fx = $derived(effects.find((e) => e.id === seg?.effect_id));
	const hasSound = $derived(device.attributes.has_audio === true && effects.some((e) => e.sound));

	const send = (change: Record<string, unknown>) =>
		void commands.send(id, lookCommand(chosen, change));
	const pick = (e: Effect) => void commands.send(id, effectCommand(chosen, e));

	// Without a catalogue, speed and intensity — what every effect had once.
	const sliders = $derived(
		fx?.sliders ?? [
			{ key: 'effect_speed', label: 'Speed', max: 255 },
			{ key: 'effect_intensity', label: 'Intensity', max: 255 }
		]
	);
	const colourSlots = $derived(
		fx?.colors ?? [
			{ slot: 0, label: 'Primary' },
			{ slot: 1, label: 'Background' },
			{ slot: 2, label: 'Third' }
		]
	);
	const COLOUR_KEY = ['color_rgb', 'color2', 'color3'];
	const QUICK = [
		['Red', '#ff0000'],
		['Orange', '#ffa000'],
		['Warm white', '#ffc187'],
		['White', '#ffffff'],
		['Green', '#08ff00'],
		['Cyan', '#00ffc8'],
		['Blue', '#0000ff'],
		['Purple', '#6a0dad'],
		['Off', '#000000']
	] as const;
	let slot = $state(0);
	const usesPalette = $derived(fx ? fx.palette !== null : true);
	let allPalettes = $state(false);
	const palettes = $derived(
		allPalettes ? (catalog?.palettes ?? []) : (catalog?.palettes ?? []).slice(0, 18)
	);
	const value = (key: string): number =>
		Number((seg as unknown as Record<string, unknown>)?.[key] ?? 0);
	const flag = (key: string): boolean =>
		(seg as unknown as Record<string, unknown>)?.[key] === true;

	// ---- saving it as a preset ----
	const presets = $derived(presetsOf(device));
	const running = $derived(
		presets.find((p) => p.id === device.attributes.preset_id && p.kind === 'look')
	);
	let naming = $state(false);
	let newName = $state('');
	function freeSlot(): number | undefined {
		const used = new Set(presets.map((p) => p.id));
		for (let i = 1; i <= 250; i++) if (!used.has(i)) return i;
		return undefined;
	}
	function saveNew() {
		const slotId = freeSlot();
		if (slotId === undefined || !newName.trim()) return;
		void commands.act(id, 'save_preset', {
			save_preset: slotId,
			preset_name: newName.trim(),
			include_brightness: true,
			include_segments: segs.length > 1
		});
		naming = false;
		newName = '';
	}
</script>

<div class="look">
	{#if segs.length > 1}
		<div class="targets" role="radiogroup" aria-label="Changes go to">
			<span class="label">Changes go to</span>
			{#each segs as s (s.id)}
				<button role="radio" aria-checked={chosen === s.id} onclick={() => (target = s.id)}
					>{s.id} · {s.name ?? `LEDs ${s.start}–${s.stop - 1}`}</button
				>
			{/each}
			<button role="radio" aria-checked={chosen === 'all'} onclick={() => (target = 'all')}
				>Every segment</button
			>
		</div>
	{/if}

	<div class="cols">
		<section class="browser">
			<div class="head">
				<h3>Effect</h3>
				<span class="num">{effects.length || '…'}</span>
			</div>
			<input
				type="search"
				placeholder="twinkle, fire, chase…"
				aria-label="Find an effect"
				bind:value={q}
			/>
			<div class="chips">
				<button aria-pressed={filter === 'all'} onclick={() => (filter = 'all')}>All</button>
				<button aria-pressed={filter === 'palette'} onclick={() => (filter = 'palette')}
					>Uses palette</button
				>
				{#if hasSound}<button aria-pressed={filter === 'sound'} onclick={() => (filter = 'sound')}
						>Sound</button
					>{/if}
				<button aria-pressed={filter === 'favourites'} onclick={() => (filter = 'favourites')}
					>Favourites</button
				>
			</div>
			<ul class="effects">
				{#if !catalog}<li class="quiet">Loading the effects…</li>{/if}
				{#each found as e (e.id)}
					<li class:on={e.id === seg?.effect_id}>
						<button class="name" {disabled} onclick={() => pick(e)}>
							<span>{e.name}</span>
							{#if e.sound}<span class="tag">sound</span>{/if}
							{#if e.two_d && !e.one_d}<span class="tag">2D</span>{/if}
							{#if e.palette !== null}<span class="paldot" aria-label="uses a palette"></span>{/if}
						</button>
						<button
							class="star"
							aria-label={favourites.has(e.id) ? `Unfavourite ${e.name}` : `Favourite ${e.name}`}
							aria-pressed={favourites.has(e.id)}
							onclick={() => toggleFavourite(e)}>{favourites.has(e.id) ? '★' : '☆'}</button
						>
					</li>
				{/each}
				{#if catalog && found.length === 0}<li class="quiet">No effect like that.</li>{/if}
			</ul>
		</section>

		<section class="controls">
			<div class="head">
				<h3>{fx?.name ?? 'Effect'}</h3>
			</div>
			{#if fx}
				<p class="quiet small">
					Uses {fx.sliders.length || 'no'} slider{fx.sliders.length === 1 ? '' : 's'}{fx.options
						.length
						? `, ${fx.options.length} option${fx.options.length === 1 ? '' : 's'}`
						: ''},
					{fx.colors.length || 'no'} colour{fx.colors.length === 1 ? '' : 's'} and {fx.palette ===
					null
						? 'no palette'
						: 'a palette'}{fx.sound ? ', and listens to sound' : ''}.
				</p>
			{/if}
			{#if sliders.length}
				<div class="sliders">
					{#each sliders as s (s.key)}
						<label>
							<span class="label">{s.label} · <span class="num">{value(s.key)}</span></span>
							<input
								type="range"
								min="0"
								max={s.max}
								value={value(s.key)}
								{disabled}
								onchange={(e) => send({ [s.key]: Number(e.currentTarget.value) })}
							/>
						</label>
					{/each}
				</div>
			{/if}
			{#if fx?.options.length}
				<div class="options">
					{#each fx.options as o (o.key)}
						<label class="check">
							<input
								type="checkbox"
								checked={flag(o.key)}
								{disabled}
								onchange={(e) => send({ [o.key]: e.currentTarget.checked })}
							/>
							{o.label}
						</label>
					{/each}
				</div>
			{/if}
			{#if colourSlots.length}
				<div class="colours">
					<span class="label">Colours</span>
					<div class="slots">
						{#each colourSlots as c (c.slot)}
							<label class="slot" class:active={slot === c.slot}>
								<input
									type="color"
									value={seg?.colors[c.slot] ?? '#000000'}
									{disabled}
									onfocus={() => (slot = c.slot)}
									onchange={(e) => send({ [COLOUR_KEY[c.slot]!]: rgbOf(e.currentTarget.value) })}
								/>
								<span>
									<b>{c.label}</b>
									<span class="num small">{seg?.colors[c.slot] ?? '—'}</span>
								</span>
							</label>
						{/each}
					</div>
					<div class="quick" aria-label="Quick colours">
						{#each QUICK as [name, hexValue] (name)}
							<button
								class="dot"
								style:background={hexValue}
								aria-label={`${name} for ${colourSlots.find((c) => c.slot === slot)?.label ?? 'the colour'}`}
								title={name}
								{disabled}
								onclick={() => send({ [COLOUR_KEY[slot]!]: rgbOf(hexValue) })}
							></button>
						{/each}
					</div>
				</div>
			{/if}
			{#if device.attributes.led_cct === true}
				<label>
					<span class="label"
						>White temperature · <span class="num">{seg?.color_temp ?? 4000} K</span></span
					>
					<input
						class="warm"
						type="range"
						min="1900"
						max="10091"
						step="32"
						value={seg?.color_temp ?? 4000}
						{disabled}
						onchange={(e) => send({ color_temp: Number(e.currentTarget.value) })}
					/>
				</label>
			{/if}
		</section>
	</div>

	{#if usesPalette && catalog}
		<section>
			<div class="head">
				<h3>{fx?.palette && fx.palette !== 'Palette' ? fx.palette : 'Palette'}</h3>
				<span class="num">{catalog.palettes.find((p) => p.id === seg?.palette_id)?.name ?? ''}</span
				>
			</div>
			<div class="palettes">
				{#each palettes as p (p.id)}
					<button
						class="pal"
						class:on={p.id === seg?.palette_id}
						{disabled}
						onclick={() => send({ palette_id: p.id })}
					>
						<span class="sw" style:background={paletteCss(p, seg?.colors ?? [])}></span>
						<span class="pname">{p.name.replace(/^\* /, '')}</span>
					</button>
				{/each}
			</div>
			{#if !allPalettes && catalog.palettes.length > 18}
				<button class="link" onclick={() => (allPalettes = true)}
					>All {catalog.palettes.length} palettes</button
				>
			{/if}
		</section>
	{/if}

	<section class="save">
		{#if naming}
			<form class="name-row" onsubmit={(e) => (e.preventDefault(), saveNew())}>
				<input
					placeholder="Name the preset"
					aria-label="Preset name"
					bind:value={newName}
					maxlength="32"
				/>
				<button class="pill" disabled={disabled || !newName.trim()}>Save</button>
				<button type="button" class="pill quiet-pill" onclick={() => (naming = false)}
					>Cancel</button
				>
			</form>
		{:else}
			<button class="pill" {disabled} onclick={() => (naming = true)}
				>Save this look as a preset…</button
			>
			{#if running}
				<button
					class="pill"
					{disabled}
					onclick={() =>
						commands.act(id, 'save_preset', {
							save_preset: running.id,
							preset_name: running.name,
							include_brightness: true,
							include_segments: segs.length > 1
						})}>Update “{running.name}”</button
				>
			{/if}
		{/if}
	</section>
</div>

<style>
	.look {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.cols {
		display: flex;
		flex-wrap: wrap;
		gap: 16px;
		align-items: flex-start;
	}
	.browser {
		flex: 1 1 260px;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.controls {
		flex: 2 1 300px;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	section {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 8px;
	}
	h3 {
		margin: 0;
		font-size: 17px;
	}
	.label {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
		letter-spacing: 0.02em;
	}
	.num {
		font-family: var(--font-num);
		font-size: 13px;
		color: var(--ink-muted);
	}
	.small {
		font-size: 13px;
	}
	.quiet {
		color: var(--ink-muted);
		margin: 0;
	}
	input[type='search'],
	.name-row input {
		font: inherit;
		min-height: 40px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 12px;
		min-width: 0;
	}
	.chips,
	.targets {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.chips button,
	.targets button {
		font: inherit;
		font-size: 14px;
		min-height: 36px;
		padding: 0 12px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.chips button[aria-pressed='true'],
	.targets button[aria-checked='true'] {
		border-color: var(--accent);
		background: var(--lit-bg);
		color: var(--lit-ink);
		font-weight: 600;
	}
	.effects {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 340px;
		overflow-y: auto;
		border: 1px solid var(--card-line);
		border-radius: var(--radius);
		background: var(--surface);
	}
	.effects li {
		display: flex;
		align-items: center;
		border-bottom: 1px solid var(--card-line);
	}
	.effects li.quiet {
		padding: 12px;
	}
	.effects li.on {
		background: var(--lit-bg);
	}
	.effects .name {
		flex: 1;
		min-width: 0;
		font: inherit;
		font-size: 15px;
		text-align: left;
		min-height: 42px;
		padding: 0 12px;
		border: none;
		background: none;
		color: var(--ink);
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.effects li.on .name {
		font-weight: 700;
		color: var(--lit-ink);
	}
	.effects .name span:first-child {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.tag {
		font-size: 11px;
		font-family: var(--font-num);
		color: var(--ink-muted);
	}
	.paldot {
		width: 16px;
		height: 8px;
		border-radius: 3px;
		background: linear-gradient(90deg, #ff0000, #ffd500, #00ff00, #0066ff, #b000ff);
		flex: none;
	}
	.star {
		width: 40px;
		min-height: 42px;
		border: none;
		background: none;
		color: var(--accent);
		font-size: 17px;
	}
	.sliders {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 12px 20px;
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	input[type='range'] {
		width: 100%;
		accent-color: var(--track-on);
		min-height: 28px;
	}
	input.warm {
		accent-color: #e0b070;
	}
	.options {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 20px;
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
	.colours {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.slots {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
	}
	.slot {
		flex-direction: row;
		align-items: center;
		gap: 10px;
		padding: 6px 12px 6px 6px;
		border: 1px solid var(--line);
		border-radius: var(--radius-chip);
		min-height: var(--target);
		box-sizing: border-box;
	}
	.slot.active {
		border-color: var(--accent);
	}
	.slot input {
		width: 40px;
		height: 32px;
		border: none;
		padding: 0;
		background: none;
	}
	.slot span {
		display: flex;
		flex-direction: column;
		font-size: 14px;
	}
	.quick {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.dot {
		width: 30px;
		height: 30px;
		border-radius: 50%;
		border: 1px solid var(--line);
	}
	.palettes {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(120px, 1fr));
		gap: 8px;
	}
	.pal {
		font: inherit;
		padding: 6px;
		border-radius: var(--radius-chip);
		border: 2px solid var(--card-line);
		background: var(--surface);
		color: var(--ink);
		text-align: left;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.pal.on {
		border-color: var(--accent);
	}
	.sw {
		display: block;
		height: 22px;
		border-radius: 5px;
	}
	.pname {
		font-size: 13px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.save {
		flex-direction: row;
		flex-wrap: wrap;
		gap: 8px;
	}
	.name-row {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		flex: 1;
	}
	.name-row input {
		flex: 1 1 200px;
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
	.quiet-pill {
		color: var(--ink-muted);
	}
	.link {
		font: inherit;
		font-size: 14px;
		border: none;
		background: none;
		color: var(--accent);
		padding: 0;
		align-self: flex-start;
		text-decoration: underline;
	}
	button:disabled {
		opacity: 0.55;
	}
</style>
