<!-- Add a thermostat: what it runs (an AC, a heater, a fan), the plug or
     switch it turns on and off (plugs and switches only, ones already run by
     a thermostat marked), the sensors that read the room (with their readings
     now) and how several combine, the temperature to keep, and the safety
     settings for its kind — then the sentence it makes, and "Add it". -->
<script>
	import {
		ago,
		choices,
		diffText,
		KINDS,
		minutes,
		nextTarget,
		targetText,
		tempText,
		thermostatsOf
	} from './thermo.js';

	let { hc, home, devices, units, onclose, ondone } = $props();

	const label = (d) => hc.devices.label(d);
	const opts = $derived(choices(home.devices, home.rooms, thermostatsOf(devices), label));

	// What a new one starts at, by kind: °C, said in the person's unit.
	const START = { ac: 20, heater: 16.5, fan: 26.5 };
	const SAFETY = {
		ac: { swing: 1, on: 180, off: 300, max: 0 },
		heater: { swing: 1, on: 120, off: 120, max: 14400 },
		fan: { swing: 2, on: 60, off: 60, max: 0 }
	};

	let kind = $state('ac');
	let plug = $state('');
	let sensors = $state([]);
	let agg = $state('average');
	let setpoint = $state(START.ac);
	let safety = $state({ ...SAFETY.ac });
	let plugQ = $state('');
	let sensorQ = $state('');
	let busy = $state(false);
	let error = $state('');

	function pickKind(k) {
		kind = k;
		setpoint = START[k];
		safety = { ...SAFETY[k] };
	}
	const narrow = (list, q) => {
		const t = q.trim().toLowerCase();
		return t ? list.filter((x) => `${x.name} ${x.room}`.toLowerCase().includes(t)) : list;
	};
	// Plugs, outlets and relays before other switches; the sensors in the
	// chosen plug's room first.
	const PLUGGY = /outlet|plug|relay|socket/i;
	const plugs = $derived(
		narrow(opts.plugs, plugQ)
			.map((p, i) => ({ p, i, k: PLUGGY.test(p.name) ? 0 : 1 }))
			.sort((x, y) => x.k - y.k || x.i - y.i)
			.map((x) => x.p)
	);
	const sens = $derived.by(() => {
		const room = opts.plugs.find((p) => p.id === plug)?.room;
		return narrow(opts.sensors, sensorQ)
			.map((s, i) => ({ s, i, k: room && s.room === room ? 0 : 1 }))
			.sort((x, y) => x.k - y.k || x.i - y.i)
			.map((x) => x.s);
	});
	const picked = $derived(opts.sensors.filter((s) => sensors.includes(s.id)));
	const thePlug = $derived(opts.plugs.find((p) => p.id === plug));

	const k = $derived(KINDS[kind]);
	const name = $derived.by(() => {
		const where = thePlug?.room || picked[0]?.room || thePlug?.name || 'New';
		const base = `${where} ${k.name} Thermostat`;
		const taken = new Set(devices.map(label));
		let n = base;
		for (let i = 2; taken.has(n); i++) n = `${base} ${i}`;
		return n;
	});
	const roomNow = $derived.by(() => {
		const vs = picked.map((s) => s.temperature).filter((v) => v !== undefined);
		if (!vs.length) return '';
		const v =
			agg === 'min'
				? Math.min(...vs)
				: agg === 'max'
					? Math.max(...vs)
					: vs.reduce((x, y) => x + y) / vs.length;
		return `The room is ${tempText(v, units)} now.`;
	});
	const sentence = $derived(
		`Turns ${thePlug?.name ?? 'the plug'} on when ${
			picked.map((s) => s.name).join(' and ') || 'the room'
		} ${kind === 'heater' ? 'falls below' : 'rises above'} ${targetText(setpoint, units)}.`
	);
	const ready = $derived(!!plug && picked.length > 0 && !busy);

	async function add() {
		busy = true;
		error = '';
		const config = {
			name,
			kind,
			sensor_device_ids: picked.map((s) => s.id),
			aggregation: agg,
			setpoint,
			hysteresis: safety.swing,
			actuator_device_id: plug,
			min_on_secs: safety.on,
			min_off_secs: safety.off,
			...(kind === 'heater' ? { max_run_secs: safety.max } : {})
		};
		const r = await hc.action('add_thermostat', { config });
		busy = false;
		if (!r.ok) error = r.error;
		else if (r.data?.status === 'error') error = r.data.error;
		else ondone?.(r.data);
	}
	const GUARDS = [0, 60, 120, 180, 300, 600];
	const SWINGS = [0.5, 1, 1.5, 2, 3];
	const LONGEST = [3600, 7200, 14400, 21600, 28800];
	const about = (s) => (s.temperature === undefined ? '—' : tempText(s.temperature, units));
</script>

<div class="add">
	<header>
		<span class="h">Add a thermostat</span>
		<span class="hc-quiet"
			>A sensor in the room, a plug or switch that runs the AC, heater or fan, and the temperature
			to keep.</span
		>
	</header>

	<section>
		<span class="step">1 · What does it run?</span>
		<div class="kinds">
			{#each [['ac', 'AC', 'Cools. Offers Cool and Off.'], ['heater', 'Heater', 'Heats. Offers Heat and Off.'], ['fan', 'Fan or other', 'Runs when it gets too warm.']] as [id, n, said] (id)}
				<button
					class="opt"
					class:on={kind === id}
					aria-pressed={kind === id}
					onclick={() => pickKind(id)}><b>{n}</b><span class="hc-quiet">{said}</span></button
				>
			{/each}
		</div>
	</section>

	<section>
		<span class="step">2 · The plug or switch it turns on and off</span>
		{#if opts.plugs.length > 9}
			<input class="find" type="search" placeholder="Find a plug or switch" bind:value={plugQ} />
		{/if}
		<div class="plugs" role="radiogroup" aria-label="Plug or switch">
			{#each plugs as p (p.id)}
				<button
					class="opt"
					class:on={plug === p.id}
					role="radio"
					aria-checked={plug === p.id}
					disabled={!!p.usedBy}
					onclick={() => (plug = p.id)}
					><b>{p.name}</b><span class="hc-quiet small"
						>{[p.room, p.usedBy ? `runs “${p.usedBy}”` : ''].filter(Boolean).join(' · ') ||
							'Not in a room'}</span
					></button
				>
			{/each}
		</div>
		<span class="hc-quiet small"
			>Plugs and switches only — not lights, locks or other thermostats. One already run by a
			thermostat is marked.</span
		>
	</section>

	<section>
		<span class="step">3 · The sensors that read the room</span>
		{#if opts.sensors.length > 8}
			<input class="find" type="search" placeholder="Find a sensor" bind:value={sensorQ} />
		{/if}
		<div class="sensors">
			{#each sens as s (s.id)}
				<label class="pick">
					<input type="checkbox" value={s.id} bind:group={sensors} />
					<span class="grow">{s.name}<span class="hc-quiet small"> {s.room}</span></span>
					<b class="hc-num">{about(s)}</b>
					{#if s.at}<span class="hc-quiet small when">{ago(s.at)}</span>{/if}
				</label>
			{/each}
		</div>
		{#if picked.length > 1}
			<label class="hc-field combine"
				><span class="hc-label">With {picked.length} sensors it uses</span>
				<select bind:value={agg}>
					<option value="average">Their average</option>
					<option value="max">The warmest</option>
					<option value="min">The coldest</option>
				</select></label
			>
		{:else}
			<span class="hc-quiet small"
				>Pick more than one to use their average (or the warmest or coldest).</span
			>
		{/if}
	</section>

	<section>
		<span class="step">4 · Keep it at</span>
		<div class="keep">
			<button
				class="round"
				aria-label="Lower"
				onclick={() => (setpoint = nextTarget(setpoint, -1, units))}>−</button
			>
			<span class="tv hc-num">{targetText(setpoint, units)}</span>
			<button
				class="round"
				aria-label="Raise"
				onclick={() => (setpoint = nextTarget(setpoint, 1, units))}>+</button
			>
			<span class="hc-quiet">{roomNow}</span>
		</div>
	</section>

	<section class="box">
		<span class="step">5 · Safety — set for {k.a}, change if you need</span>
		<div class="safety">
			<label
				><span class="hc-quiet">Swing</span>
				<select bind:value={safety.swing}>
					{#each SWINGS as s (s)}<option value={s}>± {diffText(s / 2, units)}</option>{/each}
				</select></label
			>
			<label
				><span class="hc-quiet">Once on, stays on at least</span>
				<select bind:value={safety.on}>
					{#each GUARDS as g (g)}<option value={g}>{g ? minutes(g) : 'no guard'}</option>{/each}
				</select></label
			>
			<label
				><span class="hc-quiet">Once off, stays off at least</span>
				<select bind:value={safety.off}>
					{#each GUARDS as g (g)}<option value={g}
							>{g ? minutes(g) : 'no guard'}{kind === 'ac' && g === 300
								? ' — protects the compressor'
								: ''}</option
						>{/each}
				</select></label
			>
			{#if kind === 'heater'}
				<label
					><span class="hc-quiet">Longest run</span>
					<select bind:value={safety.max}>
						{#each LONGEST as g (g)}<option value={g}>{minutes(g)}, then off and an alert</option
							>{/each}
					</select></label
				>
			{/if}
			<span class="fixed"
				><span class="hc-quiet">A sensor goes quiet after</span><span
					>2 h, then it is turned off</span
				></span
			>
			{#if kind === 'heater'}
				<span class="fixed"
					><span class="hc-quiet">When the plugin stops</span><span>turned off</span></span
				>
			{/if}
			<span class="fixed"
				><span class="hc-quiet">Switched by hand</span><span>left alone for 1 h</span></span
			>
		</div>
		{#if kind === 'heater'}
			<span class="heat-warn"
				>A heater is only as safe as its plug. If the plug has its own auto-off, set it a little
				longer than the longest run.</span
			>
		{/if}
	</section>

	{#if error}<p class="err" role="alert">{error}</p>{/if}
	<footer>
		<span class="sentence">{sentence}</span>
		<button class="hc-pill big" onclick={() => onclose?.()}>Cancel</button>
		<button class="hc-pill go big" disabled={!ready} onclick={add}
			>{busy ? 'Adding…' : 'Add it'}</button
		>
	</footer>
	<span class="hc-quiet small"
		>It's called “{name}” — rename it from its panel like any device.</span
	>
</div>

<style>
	.add {
		display: flex;
		flex-direction: column;
		gap: 22px;
		container-type: inline-size;
	}
	header {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.h {
		font-size: 22px;
		font-weight: 600;
	}
	section {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.step {
		font-weight: 600;
	}
	.kinds {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
	}
	.plugs {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 8px;
		max-height: 330px;
		overflow-y: auto;
		padding: 2px;
	}
	.opt {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 4px;
		text-align: left;
		min-height: 72px;
		padding: 12px 14px;
		border-radius: 12px;
		background: var(--surface);
		color: var(--ink);
		border: 1px solid var(--line);
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.plugs .opt {
		min-height: 60px;
	}
	.opt.on {
		border: 2px solid var(--accent);
		padding: 11px 13px;
	}
	.opt b {
		font-weight: 600;
		font-size: 15px;
	}
	.kinds .opt b {
		font-size: 16px;
	}
	.opt:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.opt .hc-quiet {
		font-size: 13px;
	}
	.small,
	.opt .small {
		font-size: 12px;
	}
	section > .hc-quiet.small {
		font-size: 13px;
	}
	.sensors {
		display: flex;
		flex-direction: column;
		gap: 6px;
		max-height: 320px;
		overflow-y: auto;
		padding: 2px;
	}
	.pick {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 44px;
		padding: 0 12px;
		border-radius: 11px;
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 14px;
	}
	.pick input {
		width: 18px;
		height: 18px;
	}
	.pick .when {
		min-width: 76px;
		text-align: right;
	}
	.grow {
		flex: 1;
		min-width: 0;
	}
	.find {
		min-height: 40px;
		border-radius: 11px;
		border: 1px solid var(--line);
		padding: 0 12px;
		background: var(--surface);
		color: var(--ink);
		font: inherit;
		max-width: 320px;
	}
	.combine select,
	.safety select {
		min-height: 40px;
		border-radius: 10px;
		padding: 0 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font: inherit;
	}
	.combine {
		max-width: 320px;
	}
	.keep {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
	}
	.round {
		width: 44px;
		height: 44px;
		border-radius: 50%;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font-size: 20px;
	}
	.tv {
		font-size: 28px;
		font-weight: 600;
		min-width: 72px;
		text-align: center;
		font-family: var(--font);
	}
	.box {
		padding: 16px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.safety {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 8px;
		font-size: 14px;
	}
	.safety label,
	.fixed {
		display: grid;
		grid-template-columns: 240px minmax(0, 1fr);
		align-items: center;
		gap: 12px;
	}
	.safety label select {
		justify-self: start;
	}
	.heat-warn {
		font-size: 13px;
		font-weight: 600;
		color: light-dark(#8a3a05, #f0b27a);
	}
	footer {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
	}
	.sentence {
		flex: 1 1 300px;
		font-size: 15px;
	}
	.big {
		min-height: 44px;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	@container (max-width: 560px) {
		.kinds,
		.plugs {
			grid-template-columns: minmax(0, 1fr);
		}
		.safety label,
		.fixed {
			grid-template-columns: minmax(0, 1fr);
			gap: 4px;
		}
	}
</style>
