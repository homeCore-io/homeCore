<svelte:options
	customElement={{
		tag: 'hc-hue-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: a scene, a look, a flash, power. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					const name = (rid) => {
						const s = h.devices.list().find((d) => d.device_type === 'scene' && d.attributes.resource_id === rid);
						return s ? h.devices.label(s) : undefined;
					};
					return sentence(value, name);
				}
			}
	}}
/>

<!-- The automation editor's step on Hue lights, rooms and scenes: apply a
     scene, set a look, flash them and put them back, or on and off — each
     with a fade. -->
<script module>
	import { build, kindOf, sentence } from './lib/step.js';
</script>

<script>
	import Wheel from './lib/Wheel.svelte';
	import Scenes from './lib/SceneTiles.svelte';
	import { adopt, effectName, FADES, fadeWords, hexToXy, kelvinToHex, paletteOf, scenesOf, xyToHex } from './lib/hue.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const first = $derived(devices.find((d) => d.device_id === deviceIds[0]));
	const type = $derived(first?.device_type);
	const roomRid = $derived(type === 'group' ? first?.attributes.group_rid : first?.attributes.room_rid);
	const scenes = $derived(scenesOf(devices, roomRid));
	const effects = $derived(
		(Array.isArray(first?.attributes.effect_values) ? first.attributes.effect_values : []).filter((e) => e !== 'no_effect')
	);

	// The step being edited, kept here so quick changes build on each other.
	let draft = $state(null);
	const v = $derived(draft ?? value ?? {});
	const kind = $derived(kindOf(v) ?? (type === 'scene' ? 'scene' : 'look'));
	function set(next) {
		draft = next;
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(deviceIds, next) } }));
	}
	const clean = (o) => Object.fromEntries(Object.entries(o).filter(([, x]) => x !== undefined && x !== '' && x !== false));

	function mode(k) {
		if (k === kind) return;
		if (k === 'scene') set(clean({ action: 'activate', scene_id: type === 'scene' ? undefined : scenes[0]?.attributes.resource_id }));
		else if (k === 'look') set({ on: true, color_temp: 2700, transition_ms: 2000 });
		else if (k === 'flash') set({ action: 'signal', kind: 'on_off', duration_secs: 5 });
		else if (k === 'power') set({ on: false });
	}

	const said = $derived.by(() => {
		const s = sentence(v, (rid) => {
			const sc = devices.find((d) => d.device_type === 'scene' && d.attributes.resource_id === rid);
			return sc ? hc.devices.label(sc) : undefined;
		});
		const names = deviceIds.map((id) => devices.find((d) => d.device_id === id)).filter(Boolean).map((d) => hc.devices.label(d));
		const who = names.length <= 2 ? names.join(' and ') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s ? s.replace('{devices}', who || 'them').replace(/^./, (c) => c.toUpperCase()) : '';
	});

	const WHITES = [2200, 2700, 4000, 5000, 6500];
	const COLOURS = ['#ff3b30', '#ff8a3d', '#ffcc00', '#4cd964', '#2bb3c0', '#3e7bfa', '#7b5cff', '#e8457a'];
	let wheel = $state(false);
	const lookColour = $derived(v.color_xy ? xyToHex(v.color_xy.x, v.color_xy.y) : null);
	const scenePicked = (s) => (type === 'scene' ? true : v.scene_id === s.attributes.resource_id);
</script>

<div class="step">
	{#if said}<p class="said">{said}</p>{/if}

	{#if type !== 'scene'}
		<span class="seg" role="group" aria-label="What to do">
			{#if scenes.length}<button class:on={kind === 'scene'} onclick={() => mode('scene')}>Apply a scene</button>{/if}
			<button class:on={kind === 'look'} onclick={() => mode('look')}>Set a look</button>
			<button class:on={kind === 'flash' || kind === 'pulse'} onclick={() => mode('flash')}>Flash them</button>
			<button class:on={kind === 'power'} onclick={() => mode('power')}>On or off</button>
		</span>
	{/if}

	{#if kind === 'scene'}
		{#if type !== 'scene'}
			<span class="hc-label">Scene</span>
			<div class="pick">
				{#each scenes as s (s.device_id)}
					<button class="scene" class:on={scenePicked(s)} onclick={() => set({ ...v, action: 'activate', scene_id: s.attributes.resource_id })}>
						<span class="pal">{#each paletteOf(s) as c, i (i)}<span style:background={c}></span>{/each}</span>
						<b>{hc.devices.label(s)}</b>
					</button>
				{/each}
			</div>
		{/if}
		<span class="hc-row opts">
			<label
				>Fade in over
				<select value={v.transition_ms ?? 0} onchange={(e) => set(clean({ ...v, transition_ms: +e.currentTarget.value || undefined }))}>
					{#each FADES as ms (ms)}<option value={ms}>{fadeWords(ms)}</option>{/each}
				</select></label
			>
			<label
				>Brightness
				<select value={v.brightness_pct ?? 0} onchange={(e) => set(clean({ ...v, brightness_pct: +e.currentTarget.value || undefined }))}>
					<option value={0}>as the scene has it</option>
					{#each [10, 25, 50, 75, 100] as b (b)}<option value={b}>{b} %</option>{/each}
				</select></label
			>
			<label class="check"
				><input type="checkbox" checked={!!v.dynamic} onchange={(e) => set(clean({ ...v, dynamic: e.currentTarget.checked }))} />Keep its colours
				moving</label
			>
		</span>
	{:else if kind === 'look'}
		<span class="hc-label">Look</span>
		<span class="hc-row">
			{#each WHITES as k (k)}
				<button class="hc-pill" class:go={v.color_temp === k} onclick={() => set(clean({ ...v, on: true, color_temp: k, color_xy: undefined, effect: undefined }))}
					><span class="dot" style:background={kelvinToHex(k)}></span>{k} K</button
				>
			{/each}
			<button class="hc-pill" class:go={!!v.color_xy} onclick={() => (wheel = !wheel)}
				><span class="dot" style:background={lookColour ?? 'conic-gradient(#f00,#ff0,#0f0,#0ff,#00f,#f0f,#f00)'}></span>A colour…</button
			>
			<button class="hc-pill" class:go={!v.color_temp && !v.color_xy && !v.effect} onclick={() => set(clean({ ...v, color_temp: undefined, color_xy: undefined, effect: undefined }))}
				>Leave the colour</button
			>
		</span>
		{#if wheel}
			<div class="hc-row">
				<Wheel value={lookColour ?? '#ff8a3d'} size={180} onpick={(hex) => set(clean({ ...v, on: true, color_xy: hexToXy(hex), color_temp: undefined, effect: undefined }))} />
				<span class="swatches">
					{#each COLOURS as c (c)}<button class="sw" style:background={c} aria-label={c} onclick={() => set(clean({ ...v, on: true, color_xy: hexToXy(c), color_temp: undefined, effect: undefined }))}></button>{/each}
				</span>
			</div>
		{/if}
		{#if effects.length && type === 'light'}
			<span class="hc-label">Or an effect</span>
			<span class="hc-row">
				{#each effects as e (e)}
					<button class="hc-pill" class:go={v.effect === e} onclick={() => set(clean({ ...v, on: true, effect: e, color_temp: undefined, color_xy: undefined }))}>{effectName(e)}</button>
				{/each}
			</span>
		{/if}
		<span class="hc-row opts">
			<label
				>Brightness
				<select
					value={v.brightness_step ? `step:${v.brightness_step}` : v.brightness_pct ?? ''}
					onchange={(e) => {
						const x = e.currentTarget.value;
						if (x.startsWith('step:')) set(clean({ ...v, brightness_pct: undefined, brightness_step: +x.slice(5) }));
						else set(clean({ ...v, brightness_step: undefined, brightness_pct: x === '' ? undefined : +x }));
					}}
				>
					<option value="">leave it</option>
					{#each [10, 25, 40, 50, 75, 100] as b (b)}<option value={b}>{b} %</option>{/each}
					<option value="step:10">10 % brighter</option>
					<option value="step:-10">10 % dimmer</option>
				</select></label
			>
			<label
				>Fade over
				<select value={v.transition_ms ?? 0} onchange={(e) => set(clean({ ...v, transition_ms: +e.currentTarget.value || undefined }))}>
					{#each FADES as ms (ms)}<option value={ms}>{fadeWords(ms)}</option>{/each}
				</select></label
			>
		</span>
	{:else if kind === 'flash' || kind === 'pulse'}
		<span class="hc-label">How</span>
		<span class="hc-row">
			<button class="hc-pill" class:go={kind === 'pulse'} onclick={() => set({ action: 'breathe' })}>Pulse once</button>
			<button class="hc-pill" class:go={v.kind === 'on_off'} onclick={() => set({ action: 'signal', kind: 'on_off', duration_secs: v.duration_secs ?? 5 })}>Blink</button>
			<button class="hc-pill" class:go={v.kind === 'on_off_color'} onclick={() => set({ action: 'signal', kind: 'on_off_color', duration_secs: v.duration_secs ?? 5, colors: [hexToXy('#ff3b30')] })}>Blink in a colour</button>
			<button class="hc-pill" class:go={v.kind === 'alternating'} onclick={() => set({ action: 'signal', kind: 'alternating', duration_secs: v.duration_secs ?? 5, colors: [hexToXy('#ff3b30'), hexToXy('#3e7bfa')] })}>Alternate two colours</button>
		</span>
		{#if kind === 'flash'}
			{#if v.colors?.length}
				<span class="hc-row">
					{#each v.colors as c, i (i)}
						<span class="hc-label">Colour {i + 1}</span>
						{#each COLOURS as hex (hex)}
							<button
								class="sw small"
								class:on={xyToHex(c.x, c.y) === xyToHex(hexToXy(hex).x, hexToXy(hex).y)}
								style:background={hex}
								aria-label={`Colour ${i + 1}: ${hex}`}
								onclick={() => set({ ...v, colors: v.colors.map((x, j) => (j === i ? hexToXy(hex) : x)) })}
							></button>
						{/each}
					{/each}
				</span>
			{/if}
			<label class="opts"
				>For
				<select value={v.duration_secs ?? 5} onchange={(e) => set({ ...v, duration_secs: +e.currentTarget.value })}>
					{#each [2, 5, 10, 30, 60] as secs (secs)}<option value={secs}>{fadeWords(secs * 1000)}</option>{/each}
				</select>
				then back to how they were</label
			>
		{/if}
	{:else if kind === 'power'}
		<span class="hc-row">
			<button class="hc-pill" class:go={v.on === true} onclick={() => set(clean({ ...v, on: true }))}>On</button>
			<button class="hc-pill" class:go={v.on === false} onclick={() => set({ ...v, on: false })}>Off</button>
			<label class="opts"
				>Fade over
				<select value={v.transition_ms ?? 0} onchange={(e) => set(clean({ ...v, transition_ms: +e.currentTarget.value || undefined }))}>
					{#each FADES as ms (ms)}<option value={ms}>{fadeWords(ms)}</option>{/each}
				</select></label
			>
		</span>
	{/if}
</div>

<style>
	.step {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.said {
		margin: 0;
		padding: 12px 14px;
		border-radius: 12px;
		background: var(--lit-bg);
		color: var(--lit-ink, var(--ink));
		font-size: 16px;
	}
	.seg {
		display: inline-flex;
		flex-wrap: wrap;
		align-self: flex-start;
		padding: 3px;
		gap: 3px;
		border-radius: 11px;
		background: var(--nav-on, var(--line));
	}
	.seg button {
		border: none;
		background: none;
		padding: 7px 14px;
		border-radius: 8px;
		font-weight: 600;
	}
	.seg button.on {
		background: var(--surface);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.1);
	}
	.pick {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
		gap: 8px;
	}
	.scene {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 8px;
		border-radius: 12px;
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
	}
	.scene.on {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.pal {
		display: flex;
		height: 28px;
		border-radius: 7px;
		overflow: hidden;
	}
	.pal span {
		flex: 1;
	}
	.scene b {
		font-size: 13px;
		font-weight: 600;
	}
	.opts,
	.opts label {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		font-size: 14px;
	}
	.opts {
		gap: 18px;
	}
	select {
		min-height: 34px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		padding: 0 8px;
	}
	.check {
		display: inline-flex;
		gap: 8px;
		align-items: center;
	}
	.dot {
		width: 14px;
		height: 14px;
		border-radius: 50%;
		border: 1px solid var(--card-line);
	}
	.swatches {
		display: grid;
		grid-template-columns: repeat(4, 38px);
		gap: 8px;
	}
	.sw {
		height: 38px;
		border-radius: 10px;
		border: 1px solid var(--card-line);
	}
	.sw.small {
		width: 30px;
		height: 30px;
	}
	.sw.on {
		box-shadow: 0 0 0 2px var(--surface), 0 0 0 4px var(--accent);
	}
</style>
