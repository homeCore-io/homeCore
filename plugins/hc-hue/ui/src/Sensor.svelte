<svelte:options
	customElement={{
		tag: 'hc-hue-sensor',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A Hue accessory's panel: a motion sensor's readings and today's motion,
     a remote's buttons and how each was last pressed, a door sensor. -->
<script>
	import { adopt, ago, num } from './lib/hue.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const kind = $derived(dev?.device_type ?? '');

	// Today's motion, and the remote's presses this week, from history.
	let motions = $state([]);
	let presses = $state([]);
	$effect(() => {
		if (!hc || !deviceId) return;
		const midnight = new Date();
		midnight.setHours(0, 0, 0, 0);
		if (kind === 'motion_sensor')
			void hc.devices
				.history(deviceId, 'motion', { from: midnight.toISOString(), limit: 2000 })
				.then((r) => (motions = r.ok ? r.data.filter((p) => p.value === true) : []));
		if (kind === 'button')
			void hc.devices
				.history(deviceId, 'last_press', { from: new Date(Date.now() - 7 * 86400_000).toISOString(), limit: 500 })
				.then((r) => (presses = r.ok ? r.data : []));
	});
	const dayFraction = (iso) => {
		const d = new Date(iso);
		return (d.getHours() * 60 + d.getMinutes()) / 1440;
	};

	const buttons = $derived(Array.isArray(a.buttons) ? a.buttons : []);
	/** Each button's last press, newest first: from history, and now. */
	const lastOf = $derived.by(() => {
		const out = {};
		const all = [...presses, ...(a.last_press ? [{ at: a.last_pressed_at ?? dev?.last_seen, value: a.last_press }] : [])];
		for (const p of all) {
			const v = String(p.value ?? '');
			const b = buttons.find((name) => v.startsWith(`${name} `));
			if (!b || v.endsWith(' down')) continue;
			if (!out[b] || Date.parse(p.at) > Date.parse(out[b].at)) out[b] = { at: p.at, how: v.slice(b.length + 1) };
		}
		return out;
	});

	let error = $state('');
	async function send(body) {
		error = '';
		const r = await hc.command(deviceId, body);
		if (!r.ok) error = r.error;
	}

	const battery = $derived(num(a.battery_pct));
	const low = $derived(battery !== undefined && battery <= 15);
	// Hue publishes °C, always; the screens have no unit setting of their own.
	const deg = '°C';
	const sens = ['Low', 'Medium', 'High'];
</script>

{#if dev}
	<div class="panel">
		<header class="titles">
			<span class="hc-quiet"
				>{[dev.room, battery !== undefined ? `battery ${Math.round(battery)} %${low ? ' — replace soon' : ''}` : '', dev.available ? '' : 'not answering']
					.filter(Boolean)
					.join(' · ')}</span
			>
		</header>
		{#if error}<p class="err" role="alert">{error}</p>{/if}

		{#if kind === 'motion_sensor'}
			<section class="hc-card">
				<div class="tiles">
					<div class="tile" class:now={a.motion === true}>
						<span class="hc-label">Motion</span>
						<b class="hc-num">{a.motion === true ? 'Now' : 'None'}</b>
						<span class="hc-quiet">{a.motion === true ? 'right now' : `last ${ago(motions.at(-1)?.at) || 'not today'}`}</span>
					</div>
					{#if num(a.temperature) !== undefined}
						<div class="tile">
							<span class="hc-label">Temperature</span>
							<b class="hc-num">{num(a.temperature).toFixed(1)}°</b>
							<span class="hc-quiet">{deg}</span>
						</div>
					{/if}
					{#if num(a.illuminance) !== undefined}
						<div class="tile">
							<span class="hc-label">Light level</span>
							<b class="hc-num">{Math.round(num(a.illuminance))}</b>
							<span class="hc-quiet">lux{num(a.illuminance) < 20 ? ' · dark' : num(a.illuminance) < 200 ? ' · dim' : ''}</span>
						</div>
					{/if}
				</div>
				<div class="hc-head"><span class="hc-label">Motion today</span><span class="hc-num">{motions.length} times</span></div>
				<div class="day" aria-label={`Motion ${motions.length} times today`}>
					{#each motions as m, i (i)}<span style:left={`${dayFraction(m.at) * 100}%`}></span>{/each}
					<i style:left={`${dayFraction(new Date().toISOString()) * 100}%`}></i>
				</div>
				<div class="hc-head"><span class="hc-quiet">midnight</span><span class="hc-quiet">noon</span><span class="hc-quiet">now</span></div>
			</section>

			<section class="hc-card">
				<h3>How it senses</h3>
				{#if num(a.motion_sensitivity) !== undefined}
					<div class="hc-head">
						<span>Sensitivity</span>
						<span class="hc-row"
							>{#each sens as label, i (i)}<button class="hc-pill" class:go={a.motion_sensitivity === i} onclick={() => send({ motion_sensitivity: i })}>{label}</button>{/each}</span
						>
					</div>
				{/if}
				<div class="hc-head">
					<span>Motion sensing</span>
					<span class="hc-row">
						<button class="hc-pill" class:go={a.enabled !== false} onclick={() => send({ enabled: true })}>On</button>
						<button class="hc-pill" class:go={a.enabled === false} onclick={() => send({ enabled: false })}>Paused</button>
					</span>
				</div>
				<p class="hc-quiet">Pausing stops its motion, temperature and light level together, as the Hue app does.</p>
			</section>
		{:else if kind === 'button'}
			<section class="hc-card">
				<div class="hc-head"><h3>Its buttons</h3><span class="hc-quiet">{a.last_pressed_at ? `last pressed ${ago(a.last_pressed_at)}` : ''}</span></div>
				<div class="buttons">
					{#each buttons as b (b)}
						<div class="btn" class:last={a.last_button === b}>
							<b>{b}</b>
							<span class="hc-quiet">{lastOf[b] ? `${lastOf[b].how} · ${ago(lastOf[b].at)}` : '—'}</span>
						</div>
					{/each}
				</div>
				<p class="hc-quiet">
					Each button is told apart, so an automation can start on “{buttons[1] ?? 'a button'} held” rather than on any press.
				</p>
			</section>
		{:else}
			<section class="hc-card">
				<div class="tiles">
					<div class="tile" class:now={a.open === true}>
						<span class="hc-label">Door</span>
						<b>{a.open === true ? 'Open' : a.open === false ? 'Closed' : '—'}</b>
						{#if a.tampered}<span class="hc-quiet">tampered with</span>{/if}
					</div>
				</div>
			</section>
		{/if}

		{#if low}
			<section class="hc-card warn">
				<h3>Battery low</h3>
				<p>{Math.round(battery)} %. Hue switches take a CR2032 coin cell; motion sensors take two AAA.</p>
			</section>
		{/if}
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.titles {
		display: flex;
		flex-direction: column;
		gap: 2px;
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
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
		gap: 10px;
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 12px;
		border-radius: 12px;
		border: 1px solid var(--card-line);
	}
	.tile b {
		font-size: 30px;
		font-weight: 500;
		line-height: 1.1;
	}
	.tile.now {
		background: var(--lit-bg);
		border-color: var(--lit-card-line, var(--card-line));
		color: var(--lit-ink, var(--ink));
	}
	.day {
		position: relative;
		height: 40px;
		border-radius: 8px;
		background: var(--bg);
	}
	.day span {
		position: absolute;
		top: 7px;
		bottom: 7px;
		width: 4px;
		margin-left: -2px;
		border-radius: 2px;
		background: var(--track-on, var(--accent));
	}
	.day i {
		position: absolute;
		top: 2px;
		bottom: 2px;
		width: 2px;
		background: var(--ink-muted);
	}
	.buttons {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 10px;
	}
	.btn {
		display: flex;
		flex-direction: column;
		gap: 3px;
		padding: 12px;
		border-radius: 12px;
		border: 1px solid var(--line);
	}
	.btn.last {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.warn {
		background: var(--alert-bg);
		border-color: var(--alert-line);
	}
	.warn h3 {
		color: var(--alert);
	}
	.warn p {
		margin: 0;
	}
</style>
