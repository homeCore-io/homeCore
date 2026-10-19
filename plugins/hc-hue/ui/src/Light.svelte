<svelte:options
	customElement={{
		tag: 'hc-hue-light',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A Hue light's panel: brightness; white, colour or its gradient; Hue's
     effects; its room's scenes; and what it is. -->
<script>
	import Wheel from './lib/Wheel.svelte';
	import Scenes from './lib/SceneTiles.svelte';
	import {
		adopt,
		effectName,
		glowOf,
		hexToXy,
		isWhite,
		kelvinToHex,
		num,
		scenesOf,
		swatchOf,
		xyToHex
	} from './lib/hue.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const light = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(light?.attributes ?? {});
	const name = $derived(light ? hc.devices.label(light) : '');

	// What it can do.
	const canColour = $derived(a.supports_color_xy === true);
	const canWhite = $derived(a.supports_color_temp === true);
	const points = $derived(num(a.gradient_points_capable) ?? 0);
	const effects = $derived((Array.isArray(a.effect_values) ? a.effect_values : []).filter((e) => e !== 'no_effect'));
	const kMin = $derived(num(a.color_temp_min) ?? 2000);
	const kMax = $derived(num(a.color_temp_max) ?? 6500);

	const bri = $derived(Math.round(num(a.brightness_pct) ?? 0));

	// Which kind of light it is showing; a person's tab choice wins until the
	// light says otherwise.
	let chosen = $state(null);
	const showing = $derived(a.color_mode === 'gradient' ? 'gradient' : isWhite(a) ? 'white' : 'colour');
	const tab = $derived(chosen ?? showing);

	let error = $state('');
	async function send(body) {
		error = '';
		const r = await hc.command(deviceId, body);
		if (!r.ok) error = r.error;
	}
	const power = () => send({ on: !a.on });

	let kHeld = $state(null);
	const kelvin = $derived(kHeld ?? num(a.color_temp) ?? 2700);
	let kTimer;
	function white(k) {
		kHeld = k;
		clearTimeout(kTimer);
		kTimer = setTimeout(async () => {
			await send({ on: true, color_temp: k, transition_ms: 400 });
			kHeld = null;
		}, 150);
	}
	const WHITES = [
		['Candlelight', 2200],
		['Warm', 2700],
		['Neutral', 4000],
		['Cool', 5000],
		['Daylight', 6500]
	];
	const whites = $derived(WHITES.filter(([, k]) => k >= kMin - 100 && k <= kMax + 100));

	const colourNow = $derived(a.color_xy && !isWhite(a) ? xyToHex(a.color_xy.x, a.color_xy.y) : '#ff8a3d');
	const colour = (hex) => send({ on: true, color_xy: hexToXy(hex), transition_ms: 400 });

	// A gradient's points: the light's own, padded to what it can show.
	let point = $state(0);
	const gradient = $derived.by(() => {
		const have = Array.isArray(a.gradient_points) ? a.gradient_points.map((p) => xyToHex(p.x, p.y)) : [];
		const n = Math.max(2, Math.min(points || 3, 5));
		const base = have.length ? have : [colourNow];
		return Array.from({ length: Math.max(have.length, Math.min(n, 3)) }, (_, i) => base[i] ?? base[base.length - 1]);
	});
	function gradientColour(hex) {
		const next = gradient.slice();
		next[point] = hex;
		send({ on: true, gradient_points: next.map(hexToXy) });
	}

	const effect = $derived(typeof a.effect === 'string' ? a.effect : 'no_effect');
	const running = $derived(effect !== 'no_effect');
	let speedHeld = $state(null);
	const speed = $derived(speedHeld ?? Math.round((num(a.dynamic_speed) ?? 0.5) * 100));
	let sTimer;
	function setSpeed(v) {
		speedHeld = v;
		clearTimeout(sTimer);
		sTimer = setTimeout(async () => {
			await send({ dynamic_speed: v / 100 });
			speedHeld = null;
		}, 200);
	}

	const scenes = $derived(scenesOf(devices, a.room_rid));
	const bridge = $derived(devices.find((d) => d.device_type === 'bridge' && d.attributes.bridge_id === a.bridge_id));
	const update = $derived(
		{ ready_to_install: 'an update is ready in the Hue app', installing: 'updating now', update_pending: 'an update is on its way', no_update: 'up to date' }[a.update_state] ?? ''
	);
	const stateLine = $derived.by(() => {
		if (!light) return '';
		if (!light.available) return 'Not answering the bridge';
		if (!a.on) return 'Off';
		const what = running ? effectName(effect) : tab === 'white' ? `${Math.round(num(a.color_temp) ?? 0)} K` : tab === 'gradient' ? 'gradient' : 'colour';
		return `On · ${bri} % · ${what}`;
	});
</script>

{#if light}
	<div class="panel">
		<!-- The app's panel above has its name, power, brightness and warmth. -->
		<div class="hc-head intro">
			<span class="hc-quiet">{[a.product_name, stateLine].filter(Boolean).join(' · ')}</span>
			{#if a.supports_identify}
				<button class="hc-pill" onclick={() => send({ action: 'identify' })}>Identify</button>
			{/if}
		</div>
		{#if error}<p class="err" role="alert">{error}</p>{/if}

		<div class="cols">
			<div class="main">
				{#if (canWhite && canColour) || points > 1 || canColour}
					<section class="hc-card">
						<div class="hc-head">
							<h3>Light</h3>
							{#if canColour}
								<span class="seg" role="group" aria-label="Kind of light">
									{#if canWhite}<button class:on={tab === 'white'} onclick={() => (chosen = 'white')}>White</button>{/if}
									<button class:on={tab === 'colour'} onclick={() => (chosen = 'colour')}>Colour</button>
									{#if points > 1}<button class:on={tab === 'gradient'} onclick={() => (chosen = 'gradient')}>Gradient</button>{/if}
								</span>
							{/if}
						</div>

						{#if tab === 'white' && canWhite}
							<span class="hc-quiet">From {kMin} K, warm, to {kMax} K, cool — or one of these:</span>
							<span class="hc-row">
								{#each whites as [label, k] (k)}
									<button class="hc-pill" class:go={Math.abs(kelvin - k) < 150} onclick={() => white(Math.min(kMax, Math.max(kMin, k)))}
										><span class="dot" style:background={kelvinToHex(k)}></span>{label}</button
									>
								{/each}
							</span>
						{:else if tab === 'colour'}
							<div class="colour">
								<Wheel value={colourNow} onpick={colour} />
								<div class="swatches">
									<span class="hc-label">From its room's scenes</span>
									<span class="grid">
										{#each [...new Set(scenes.flatMap((s) => s.attributes.palette ?? []))].slice(0, 12) as hex (hex)}
											<button class="sw" style:background={hex} aria-label={`Colour ${hex}`} onclick={() => colour(hex)}></button>
										{/each}
									</span>
								</div>
							</div>
						{:else if tab === 'gradient'}
							<div class="strip" style:background={`linear-gradient(90deg, ${gradient.join(', ')})`}></div>
							<span class="points">
								{#each gradient as hex, i (i)}
									<button class="point" class:on={point === i} onclick={() => (point = i)}>
										<span style:background={hex}></span>
										<span class="hc-quiet">Point {i + 1}</span>
									</button>
								{/each}
							</span>
							<Wheel value={gradient[point]} onpick={gradientColour} />
							<p class="hc-quiet">This lamp shows up to {points} colours along its length. Tap a point, then its colour.</p>
						{/if}
					</section>
				{/if}

				{#if effects.length}
					<section class="hc-card">
						<div class="hc-head"><h3>Effects</h3></div>
						<span class="hc-row">
							<button class="hc-pill" class:go={!running} onclick={() => send({ effect: 'no_effect' })}>None</button>
							{#each effects as e (e)}
								<button class="hc-pill" class:go={effect === e} onclick={() => send({ on: true, effect: e })}>{effectName(e)}</button>
							{/each}
						</span>
						{#if running}
							<div class="hc-head"><span class="hc-label">Speed</span><span class="hc-num">{speed} %</span></div>
							<input type="range" min="0" max="100" value={speed} aria-label="Effect speed" oninput={(e) => setSpeed(+e.currentTarget.value)} />
						{/if}
					</section>
				{/if}
			</div>

			<div class="side">
				{#if scenes.length}
					<section class="hc-card">
						<div class="hc-head"><h3>{a.room_name ?? 'Its room'}'s scenes</h3></div>
						<Scenes {hc} {scenes} />
					</section>
				{/if}
				<section class="hc-card">
					<h3>About it</h3>
					<dl>
						{#if a.product_name}<dt>Model</dt><dd>{a.product_name}{a.model_id ? ` · ${a.model_id}` : ''}</dd>{/if}
						{#if bridge}<dt>Bridge</dt><dd>{hc.devices.label(bridge)}</dd>{/if}
						<dt>Signal</dt><dd>{light.available ? 'Connected' : 'Not answering — switched off at the wall?'}</dd>
						{#if a.sw_version}<dt>Software</dt><dd>{a.sw_version}{update ? ` · ${update}` : ''}</dd>{/if}
					</dl>
				</section>
			</div>
		</div>
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 16px;
		container-type: inline-size;
	}
	.who {
		display: flex;
		gap: 14px;
		align-items: center;
		min-width: 0;
	}
	.glow {
		width: 56px;
		height: 56px;
		border-radius: 16px;
		flex: none;
		border: 1px solid var(--card-line);
	}
	.titles {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	h2 {
		margin: 0;
		font-size: 22px;
	}
	h3 {
		margin: 0;
		font-size: 16px;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.cols {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 16px;
	}
	@container (min-width: 760px) {
		.cols {
			grid-template-columns: minmax(0, 1.7fr) minmax(0, 1fr);
			align-items: start;
		}
	}
	.main,
	.side {
		display: flex;
		flex-direction: column;
		gap: 16px;
		min-width: 0;
	}
	input[type='range'] {
		width: 100%;
	}
	.track {
		appearance: none;
		height: 40px;
		border-radius: 12px;
		background: linear-gradient(90deg, #2a2620, var(--fill, #ffd8a3));
		border: 1px solid var(--card-line);
	}
	.track::-webkit-slider-thumb {
		appearance: none;
		width: 30px;
		height: 34px;
		border-radius: 9px;
		background: #fff;
		box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
	}
	.track::-moz-range-thumb {
		width: 30px;
		height: 34px;
		border-radius: 9px;
		background: #fff;
		border: none;
		box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
	}
	.ends {
		justify-content: space-between;
	}
	.dot {
		width: 14px;
		height: 14px;
		border-radius: 50%;
		border: 1px solid var(--card-line);
	}
	.seg {
		display: inline-flex;
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
	.colour {
		display: flex;
		flex-wrap: wrap;
		gap: 20px;
		align-items: center;
	}
	.swatches {
		display: flex;
		flex-direction: column;
		gap: 8px;
		flex: 1;
		min-width: 200px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(6, minmax(0, 1fr));
		gap: 8px;
	}
	.sw {
		height: 38px;
		border-radius: 10px;
		border: 1px solid var(--card-line);
	}
	.strip {
		height: 52px;
		border-radius: 12px;
	}
	.points {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(90px, 1fr));
		gap: 8px;
	}
	.point {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 8px;
		border-radius: 12px;
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
	}
	.point.on {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.point span:first-child {
		height: 28px;
		border-radius: 8px;
	}
	dl {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: 6px 14px;
		margin: 0;
		font-size: 14px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
	}
</style>
