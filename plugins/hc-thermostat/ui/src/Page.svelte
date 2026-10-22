<svelte:options
	customElement={{ tag: 'hc-thermostat-page', props: { host: { type: 'Object' } } }}
/>

<!-- The Thermostat plugin's page: how many thermostats and how many need
     someone, "+ Add a thermostat"; a card per thermostat — the room, the
     target, what it is doing and why, its sensor and switch, today, its
     guards, Open and Turn off; any thermostat in Settings that can't run, with
     the one-press fix where there is one; and what happens when something
     goes wrong. -->
<script>
	import Add from './lib/Add.svelte';
	import {
		activeMode,
		adopt,
		ago,
		attrs,
		house,
		kindOf,
		kindWords,
		MODE_NAMES,
		minutes,
		needsYou,
		newest,
		nowOf,
		pageLine,
		readingsOf,
		settingsHref,
		targetNow,
		targetText,
		tempText,
		thermostatsOf,
		todayLine
	} from './lib/thermo.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const therms = $derived(thermostatsOf(devices, label));
	const canRun = $derived(!!hc?.me.can('devices:command'));
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));

	let units = $state({ temperature: 'C' });
	$effect(() => {
		if (!hc?.units) return;
		units = { temperature: hc.units.temperature };
		return hc.units.subscribe?.((u) => (units = u));
	});
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 5000);
		return () => clearInterval(t);
	});

	let home = $state({ devices: [], rooms: [], names: {} });
	async function readHouse(fresh = false) {
		home = await house(hc, { fresh });
	}
	// The entries in Settings that can't run, from the plugin.
	let status = $state(null);
	async function readStatus() {
		const r = await hc.query('status');
		if (r.ok) status = r.data;
	}
	$effect(() => {
		if (!hc) return;
		void readHouse();
		void readStatus();
		const t = setInterval(() => {
			void readStatus();
			void readHouse(true);
		}, 30000);
		return () => clearInterval(t);
	});
	const names = $derived(home.names);
	const called = (id) => names[id] ?? id;
	const skipped = $derived(status?.skipped ?? []);
	const needs = $derived(therms.filter(needsYou));

	let adding = $state(false);
	let error = $state('');
	let said = $state('');
	async function run(p) {
		error = '';
		const r = await p;
		if (r && !r.ok) error = r.error;
		else if (r?.data?.status === 'error') error = r.data.error;
		return r;
	}
	async function fix(s) {
		const r = await run(
			hc.action('fix_entry', { index: s.index, id: s.id, setpoint: s.fix.celsius })
		);
		if (r?.ok && r.data?.status !== 'error') {
			said = `“${s.name ?? s.id}” is now ${s.fix.celsius} °C — the plugin restarts with it.`;
			setTimeout(readStatus, 4000);
		}
	}
	async function remove(d) {
		const ok = await hc.ui.confirm({
			title: `Remove ${label(d)}?`,
			body: 'Its switch is turned off first if the thermostat was running it.',
			lost: ['Its place in automations and on dashboards', 'Its history'],
			go: 'Remove',
			danger: true
		});
		if (!ok) return;
		const id = d.device_id.replace(/^thermostat_/, '');
		const r = await run(hc.action('remove_thermostat', { id }));
		if (r?.ok && r.data?.status !== 'error') said = `${label(d)} was removed.`;
	}
	function added(r) {
		adding = false;
		said = `Added. It starts at once; the plugin restarts a moment later to read its settings.`;
		void readHouse(true);
		if (r?.device_id) setTimeout(() => hc.ui.openDevice(r.device_id), 3500);
	}

	const pill = $derived(
		!therms.length
			? 'No thermostats yet'
			: `${therms.length} thermostat${therms.length === 1 ? '' : 's'}${
					needs.length
						? ` · ${needs.length} need${needs.length === 1 ? 's' : ''} you`
						: ' · all well'
				}`
	);
	const sensorLine = (d) => {
		const rs = readingsOf(d);
		if (!rs.length) return 'none chosen';
		const r = rs.find((x) => !x.stale) ?? newest(d) ?? rs[0];
		const more = rs.length > 1 ? ` (+${rs.length - 1})` : '';
		return `${called(r.id)}${more} · ${r.at ? ago(r.at, now) : 'no reading yet'}`;
	};
	const switchLine = (d) => {
		const a = attrs(d);
		if (!a.actuator_device_id) return 'nothing chosen';
		const st = a.actuator_online === false ? 'offline' : a.actuator_state ? 'on' : 'off';
		const why = a.reason === 'sensors_stale' && !a.actuator_state ? ' (turned off for safety)' : '';
		const unconfirmed = a.actuator_confirmed === false ? ' · not confirmed' : '';
		return `${called(a.actuator_device_id)} · ${st}${why}${unconfirmed}`;
	};
	const guardsLine = (d) => {
		const a = attrs(d);
		if (kindOf(d) === 'heater')
			return `${a.max_run_secs ? `longest run ${minutes(a.max_run_secs)} · ` : ''}off when the plugin stops`;
		if (!a.min_on_secs && !a.min_off_secs) return 'none — it can switch at any time';
		return `on at least ${minutes(a.min_on_secs)}, off at least ${minutes(a.min_off_secs)}`;
	};
</script>

<div class="page">
	{#if adding}
		<Add {hc} {home} devices={therms} {units} onclose={() => (adding = false)} ondone={added} />
	{:else}
		<header class="top">
			<span class="pill" class:warn={needs.length} class:none={!therms.length}
				><span class="dot"></span>{pill}</span
			>
			<span class="grow"></span>
			{#if admin}<button
					class="hc-pill go big"
					onclick={() => {
						said = '';
						void readHouse(true);
						adding = true;
					}}>+ Add a thermostat</button
				>{/if}
		</header>
		{#if error}<p class="err" role="alert">{error}</p>{/if}
		{#if said}<p class="ok" role="status">{said}</p>{/if}

		<div class="grid">
			{#each therms as d (d.device_id)}
				{@const a = attrs(d)}
				{@const k = kindWords(d)}
				{@const n = nowOf(d, { units, names, now })}
				<div class="card" class:needs={needsYou(d)}>
					<div class="head">
						<div class="who">
							<b>{label(d)}</b>
							<span class="hc-quiet">{k.name} · {k.does}{d.room ? ` · ${d.room}` : ''}</span>
						</div>
						<div class="temp">
							<span class="big-temp hc-num">{tempText(a.temperature, units)}</span>
							<span class="hc-quiet"
								>{a.hvac_mode === 'off'
									? 'Off'
									: `to ${targetText(targetNow(d), units)} · ${MODE_NAMES[a.hvac_mode]}`}</span
							>
						</div>
					</div>
					<span class="now {n.tone}">{pageLine(d, { units, names, now })}</span>
					<dl class="facts">
						<dt>Sensor</dt>
						<dd>{sensorLine(d)}</dd>
						<dt>Switches</dt>
						<dd>{switchLine(d)}</dd>
						<dt>Today</dt>
						<dd>{todayLine(d, now)}</dd>
						<dt>Guards</dt>
						<dd>{guardsLine(d)}</dd>
					</dl>
					<div class="hc-row">
						<button class="hc-pill" onclick={() => hc.ui.openDevice(d.device_id)}>Open</button>
						{#if canRun && d.available !== false}
							{#if a.hvac_mode === 'off'}
								<button
									class="hc-pill"
									onclick={() => run(hc.command(d.device_id, { hvac_mode: activeMode(d) }))}
									>Turn on — {MODE_NAMES[activeMode(d)]}</button
								>
							{:else}
								<button
									class="hc-pill"
									onclick={() => run(hc.command(d.device_id, { hvac_mode: 'off' }))}
									>Turn off</button
								>
							{/if}
						{/if}
						{#if admin}<button class="link danger" onclick={() => remove(d)}>Remove…</button>{/if}
					</div>
				</div>
			{/each}
		</div>
		{#if !therms.length && !skipped.length}
			<p class="hc-quiet">
				No thermostats yet. One needs a temperature sensor in the room and the plug or switch that
				runs the AC, heater or fan.
			</p>
		{/if}

		{#each skipped as s (s.index)}
			<section class="box warnbox">
				<strong>Not running: “{s.name ?? s.id ?? `thermostat ${s.index + 1}`}” in Settings</strong>
				<span class="text"
					>{s.problem.replace(/^./, (c) => c.toUpperCase())}{s.fix ? ', so it was left off' : ''}.
					The other thermostats run as normal.</span
				>
				{#if admin}
					<div class="hc-row">
						{#if s.fix}
							<button class="hc-pill go" onclick={() => fix(s)}
								>Make it {s.fix.fahrenheit} °F ({s.fix.celsius} °C)</button
							>
						{/if}
						<button class="hc-pill" onclick={() => hc.ui.navigate(settingsHref(hc))}
							>Open Settings</button
						>
					</div>
				{/if}
			</section>
		{/each}

		<section class="box">
			<strong>When something goes wrong</strong>
			<div class="wrong">
				<div>
					<b>A sensor goes quiet</b><span
						>After 2 h with no reading it no longer counts. With none left, the switch is turned off
						and an alert says so, until a reading comes back.</span
					>
				</div>
				<div>
					<b>The switch doesn't answer</b><span
						>Every on and off is checked. One the plug never confirms, or a plug that's offline,
						raises an alert and is tried again.</span
					>
				</div>
				<div>
					<b>Someone flips the switch</b><span
						>Taken as a manual override: left alone for 1 h (or until the target or mode changes),
						then the thermostat takes over again.</span
					>
				</div>
				<div>
					<b>Heaters, always</b><span
						>Turned off when the plugin stops or a thermostat is removed, and never left on longer
						than their longest run (4 h unless you change it). A plug with its own auto-off is safer
						still.</span
					>
				</div>
			</div>
		</section>
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 22px;
		container-type: inline-size;
	}
	.top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
	}
	.grow {
		flex: 1 1 200px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		padding: 6px 12px;
		border-radius: 999px;
		font-size: 14px;
		font-weight: 600;
		background: light-dark(#e6f0e1, #1f3a26);
		color: light-dark(#2f5a27, #8fd19e);
	}
	.pill .dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: currentColor;
	}
	.pill.warn {
		background: light-dark(#fbe6d4, #3a2a1a);
		color: light-dark(#8a3a05, #f0b27a);
	}
	.pill.warn .dot {
		background: #b5530c;
	}
	.pill.none {
		background: var(--surface);
		color: var(--ink-muted);
	}
	.big {
		min-height: 44px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(330px, 1fr));
		gap: 14px;
		align-items: start;
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		min-width: 0;
	}
	.card.needs {
		border: 2px solid #d9822b;
		padding: 15px;
	}
	.head {
		display: flex;
		align-items: flex-start;
		gap: 12px;
	}
	.who {
		display: flex;
		flex-direction: column;
		gap: 2px;
		flex: 1;
		min-width: 0;
	}
	.who b {
		font-weight: 600;
	}
	.who .hc-quiet,
	.temp .hc-quiet {
		font-size: 13px;
	}
	.temp {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		text-align: right;
	}
	.big-temp {
		font-size: 30px;
		font-weight: 600;
		line-height: 1;
		font-family: var(--font);
	}
	.now {
		font-size: 14px;
	}
	.now.cool {
		font-weight: 600;
		color: light-dark(#1f5a85, #8cc4ec);
	}
	.now.heat {
		font-weight: 600;
		color: light-dark(#a3470a, #f0a46b);
	}
	.now.warn {
		font-weight: 600;
		color: light-dark(#8a3a05, #f0b27a);
	}
	.now.quiet {
		color: var(--ink-muted);
		font-weight: 600;
	}
	.facts {
		display: grid;
		grid-template-columns: 96px minmax(0, 1fr);
		row-gap: 6px;
		margin: 0;
		font-size: 13px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.hc-row {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.card .hc-pill {
		min-height: 40px;
	}
	.link {
		border: none;
		background: none;
		padding: 0 6px;
		min-height: 40px;
		font-size: 14px;
		color: var(--ink-muted);
		margin-left: auto;
	}
	.link.danger:hover {
		color: var(--alert);
	}
	.box {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.box strong {
		font-weight: 600;
	}
	.warnbox {
		background: light-dark(#fbf3e4, #2f2818);
		border-color: light-dark(#efdcbb, #4a3d22);
	}
	.text {
		font-size: 14px;
		color: color-mix(in srgb, var(--ink) 80%, var(--ink-muted));
	}
	.box .hc-pill {
		min-height: 40px;
	}
	.wrong {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
		gap: 12px;
		font-size: 14px;
		color: color-mix(in srgb, var(--ink) 80%, var(--ink-muted));
	}
	.wrong > div {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.wrong b {
		font-weight: 600;
		color: var(--ink);
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.ok {
		margin: 0;
		font-size: 14px;
	}
	@container (max-width: 520px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
