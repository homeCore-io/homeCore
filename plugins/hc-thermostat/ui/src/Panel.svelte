<svelte:options
	customElement={{
		tag: 'hc-thermostat-panel',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A thermostat's panel: the room's temperature in a ring, the target with
     − and + (half a degree Celsius a step, shown in the person's unit), the
     modes its kind has; what it is doing now and why, with the button that
     helps; one-tap holds; today's chart; what it reads, what it switches, and
     how it switches — each with a way to change it. The app draws the name and
     room above. -->
<script>
	import Chart from './lib/Chart.svelte';
	import {
		act,
		activeMode,
		adopt,
		ago,
		attrs,
		band,
		choices,
		clock,
		diffText,
		holdChips,
		holdSaid,
		house,
		kindOf,
		kindWords,
		MODE_NAMES,
		minutes,
		nextTarget,
		nowOf,
		num,
		onlyNote,
		readingsOf,
		span,
		targetNow,
		targetText,
		tempText,
		thermostatsOf,
		todayLine
	} from './lib/thermo.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(attrs(dev));
	const k = $derived(kindWords(dev));

	let units = $state({ temperature: 'C' });
	$effect(() => {
		if (!hc?.units) return;
		units = { temperature: hc.units.temperature };
		return hc.units.subscribe?.((u) => (units = u));
	});

	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});

	// Other devices' names and rooms: the sensors and plugs it uses.
	let home = $state({ devices: [], rooms: [], names: {} });
	$effect(() => {
		if (!hc) return;
		let gone = false;
		const load = (fresh) =>
			house(hc, { fresh }).then((h) => {
				if (!gone) home = h;
			});
		void load(false);
		const t = setInterval(() => load(true), 60000);
		return () => {
			gone = true;
			clearInterval(t);
		};
	});
	const names = $derived(home.names);
	const called = (id) => names[id] ?? id;

	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);
	const n = $derived(dev ? nowOf(dev, { units, names, now }) : null);
	const holding = $derived(num(a.hold_setpoint) !== undefined);

	let error = $state('');
	async function send(body) {
		error = '';
		const r = await hc.command(deviceId, body);
		if (!r.ok) error = r.error;
		return r;
	}

	// The target: − and + move a shown value at once, and send it once the
	// presses stop. During a hold they move the hold's target.
	let want = $state(null);
	let timer;
	const shownTarget = $derived(want ?? targetNow(dev));
	function nudge(dir) {
		want = nextTarget(shownTarget, dir, units);
		clearTimeout(timer);
		timer = setTimeout(sendTarget, 700);
	}
	async function sendTarget() {
		const v = want;
		if (v === null) return;
		const left = Math.round((Date.parse(a.hold_until) - Date.now()) / 1000);
		const r =
			holding && left > 60
				? await send({ action: 'hold', setpoint: v, for_secs: left })
				: await send({ action: 'set_setpoint', value: v });
		if (!r.ok) want = null;
	}
	$effect(() => {
		if (want !== null && Math.abs((targetNow(dev) ?? NaN) - want) < 0.01) want = null;
	});

	const ring = $derived(
		a.hvac_action === 'cooling' ? 'cool' : a.hvac_action === 'heating' ? 'heat' : ''
	);
	const reads = $derived(dev ? readingsOf(dev) : []);
	const current = $derived(reads.find((r) => !r.stale));
	const bandLine = $derived.by(() => {
		if (current) return `${called(current.id)} · ${ago(current.at, now)}`;
		const last = reads.filter((r) => r.at).sort((x, y) => Date.parse(y.at) - Date.parse(x.at))[0];
		if (last?.temperature !== undefined)
			return `last read ${tempText(last.temperature, units)} at ${clock(last.at)}`;
		return 'no reading yet';
	});

	const chips = $derived(dev ? holdChips(dev, units, now) : []);

	const sensorsWords = $derived.by(() => {
		const several = reads.length > 1;
		const how = { average: 'average', min: 'coldest', max: 'warmest' }[a.aggregation] ?? 'average';
		const quiet = minutes(a.sensor_timeout_secs ?? 7200);
		return several
			? `${reads.length} sensors, combined by their ${how}. A sensor quiet for ${quiet} no longer counts.`
			: `One sensor. With several, it uses their average (or the warmest or coldest). A sensor quiet for ${quiet} no longer counts.`;
	});
	const plugWords = $derived.by(() => {
		const at = a.actuator_last_change ? clock(a.actuator_last_change) : '';
		const did = a.actuator_state ? 'on' : 'off';
		if (!a.actuator_device_id) return 'Nothing chosen yet.';
		if (a.actuator_online === false) return "It's offline, so it can't be told anything.";
		if (a.hvac_mode === 'off') return 'Not controlled while the thermostat is off.';
		if (a.reason === 'override') return `Turned ${did} by hand${at ? ` at ${at}` : ''}.`;
		if (a.reason === 'sensors_stale' && !a.actuator_state)
			return `Turned off${at ? ` at ${at}` : ''} for safety${a.actuator_confirmed ? '; the plug confirmed it' : ''}.`;
		if (!a.actuator_confirmed)
			return `Asked to turn ${did}${at ? ` at ${at}` : ''} — not confirmed yet.`;
		return at ? `Turned ${did} at ${at} and the plug confirmed it.` : `It is ${did}.`;
	});
	const swing = $derived.by(() => {
		const b = band(dev);
		const [on, off] = activeMode(dev) === 'cool' ? [b.high, b.low] : [b.low, b.high];
		return `± ${diffText(b.half, units)} — on at ${tempText(on, units)}, off at ${tempText(off, units)}`;
	});

	// ---- changing what it reads, switches and how ----
	let editing = $state('');
	let pickSensors = $state([]);
	let pickAgg = $state('average');
	let pickPlug = $state('');
	let pickSwing = $state(1);
	let pickOn = $state(0);
	let pickOff = $state(0);
	const opts = $derived(
		choices(home.devices, home.rooms, thermostatsOf(devices), (d) => hc.devices.label(d))
	);
	function edit(what) {
		editing = editing === what ? '' : what;
		pickSensors = reads.map((r) => r.id);
		pickAgg = a.aggregation ?? 'average';
		pickPlug = a.actuator_device_id ?? '';
		pickSwing = num(a.hysteresis) ?? 1;
		pickOn = num(a.min_on_secs) ?? 0;
		pickOff = num(a.min_off_secs) ?? 0;
		if (what) void house(hc, { fresh: true }).then((h) => (home = h));
	}
	async function save() {
		let r = { ok: true };
		if (editing === 'sensors') {
			r = await send({ action: 'set_sensors', sensor_ids: pickSensors });
			if (r.ok && pickAgg !== a.aggregation)
				r = await send({ action: 'set_aggregation', value: pickAgg });
		} else if (editing === 'switch') {
			r = await send({ action: 'set_actuator', device_id: pickPlug });
		} else if (editing === 'guards') {
			if (pickSwing !== num(a.hysteresis))
				r = await send({ action: 'set_hysteresis', value: pickSwing });
			if (r.ok)
				r = await send({ action: 'set_short_cycle', min_on_secs: pickOn, min_off_secs: pickOff });
		}
		if (r.ok) editing = '';
	}
	const GUARDS = [0, 60, 120, 180, 300, 600, 900];
	const SWINGS = [0.5, 1, 1.5, 2, 3];
</script>

{#if dev}
	<div class="panel" class:off={dev.available === false}>
		<p class="kind hc-quiet">{k.name} · {k.does}</p>
		<div class="top">
			<section class="box dial">
				<div class="ring {ring}">
					<span class="hc-quiet small">In the room</span>
					<span class="room hc-num">{tempText(a.temperature, units)}</span>
					<span class="hc-quiet">{bandLine}</span>
				</div>
				<div class="target">
					<button
						class="round"
						aria-label="Lower the target"
						disabled={!canRun || a.hvac_mode === 'off'}
						onclick={() => nudge(-1)}>−</button
					>
					<span class="tv"
						><span class="hc-quiet small">{holding ? 'Holding' : 'Target'}</span><b class="hc-num"
							>{targetText(shownTarget, units)}</b
						></span
					>
					<button
						class="round"
						aria-label="Raise the target"
						disabled={!canRun || a.hvac_mode === 'off'}
						onclick={() => nudge(1)}>+</button
					>
				</div>
				<span class="seg" role="group" aria-label="Mode">
					{#each k.modes as m (m)}
						<button
							class:on={a.hvac_mode === m}
							aria-pressed={a.hvac_mode === m}
							disabled={!canRun}
							onclick={() => a.hvac_mode !== m && send({ hvac_mode: m })}>{MODE_NAMES[m]}</button
						>
					{/each}
				</span>
				<span class="hc-quiet note">{onlyNote(dev)}</span>
			</section>

			<div class="side">
				{#if n}
					<section class="box now" class:warnbox={n.tone === 'warn'}>
						<span class="cap">Doing now</span>
						<span class="title {n.tone}">{n.title}</span>
						<span class="why">{n.why}</span>
						{#if n.act && canRun}
							<button class="hc-pill" onclick={() => act(hc, dev, n.act.kind)}>{n.act.label}</button
							>
						{/if}
					</section>
				{/if}
				<section class="box">
					<span class="cap">Hold a different target for a while</span>
					<div class="chips">
						{#each chips as c (c.id)}
							<button
								class="chip"
								class:on={holding && a.hold_setpoint === c.setpoint}
								disabled={!canRun || a.hvac_mode === 'off'}
								onclick={() => send({ action: 'hold', setpoint: c.setpoint, for_secs: c.for_secs })}
								>{c.name}</button
							>
						{/each}
						{#if holding}
							<button class="chip" disabled={!canRun} onclick={() => send({ action: 'resume' })}
								>Back to {targetText(a.setpoint, units)}</button
							>
						{/if}
					</div>
					<span class="hc-quiet said">{holdSaid(dev, units)}</span>
				</section>
			</div>
		</div>
		{#if error || a.last_command_error}
			<p class="err" role="alert">{error || a.last_command_error}</p>
		{/if}

		<section class="box">
			<div class="head">
				<b>Today</b><span class="hc-quiet"
					>{todayLine(dev, now)}{num(a.longest_run_secs_today)
						? ` · longest ${span(a.longest_run_secs_today)}`
						: ''}</span
				>
			</div>
			<Chart {hc} {dev} {units} />
			<span class="hc-quiet small"
				>Shaded: when {k.the} was on. From homeCore's own history of this thermostat.</span
			>
		</section>

		<div class="two">
			<section class="box">
				<b>Reads</b>
				{#each reads as r (r.id)}
					<div class="line">
						<span class="hc-dot" class:good={!r.stale} class:warn={r.stale}></span>
						<span class="grow">{called(r.id)}</span>
						<span class="hc-num">{tempText(r.temperature, units)}</span>
						<span class="hc-quiet age"
							>{!r.online
								? 'offline'
								: r.stale
									? r.at
										? `quiet ${span((now - Date.parse(r.at)) / 1000)}`
										: 'no reading'
									: ago(r.at, now)}</span
						>
					</div>
				{/each}
				<span class="hc-quiet small">{sensorsWords}</span>
				{#if canRun}<button class="link" onclick={() => edit('sensors')}>Change sensors ›</button
					>{/if}
				{#if editing === 'sensors'}
					<div class="editor">
						{#each opts.sensors as s (s.id)}
							<label class="pick">
								<input type="checkbox" value={s.id} bind:group={pickSensors} />
								<span class="grow">{s.name}<span class="hc-quiet small"> {s.room}</span></span>
								<span class="hc-num">{tempText(s.temperature, units)}</span>
							</label>
						{/each}
						<label class="hc-field"
							><span class="hc-label">With several</span>
							<select bind:value={pickAgg}>
								<option value="average">Their average</option>
								<option value="max">The warmest</option>
								<option value="min">The coldest</option>
							</select></label
						>
						<div class="hc-row">
							<button class="hc-pill go" disabled={!pickSensors.length} onclick={save}>Save</button>
							<button class="hc-pill" onclick={() => (editing = '')}>Cancel</button>
						</div>
					</div>
				{/if}
			</section>
			<section class="box">
				<b>Switches</b>
				<div class="line">
					<span
						class="hc-dot"
						class:good={a.actuator_online !== false && a.actuator_confirmed}
						class:warn={a.actuator_online === false || !a.actuator_confirmed}
					></span>
					<span class="grow">{a.actuator_device_id ? called(a.actuator_device_id) : 'Nothing'}</span
					>
					<b>{a.actuator_state ? 'On' : 'Off'}</b>
				</div>
				<span class="hc-quiet small">{plugWords}</span>
				{#if canRun}
					<div class="hc-row">
						{#if a.hvac_mode !== 'off' && a.actuator_device_id}
							<button class="hc-pill" onclick={() => send({ action: 'resync' })}
								>Send it again</button
							>
						{/if}
						<button class="link" onclick={() => edit('switch')}>Change switch ›</button>
					</div>
				{/if}
				{#if editing === 'switch'}
					<div class="editor">
						{#each opts.plugs as p (p.id)}
							<label class="pick">
								<input type="radio" name="plug" value={p.id} bind:group={pickPlug} />
								<span class="grow"
									>{p.name}<span class="hc-quiet small">
										{p.room}{p.usedBy && p.id !== a.actuator_device_id
											? ` · runs “${p.usedBy}”`
											: ''}</span
									></span
								>
							</label>
						{/each}
						<span class="hc-quiet small"
							>Plugs and switches only. The one it switches now is turned off first if it is on.</span
						>
						<div class="hc-row">
							<button class="hc-pill go" disabled={!pickPlug} onclick={save}>Save</button>
							<button class="hc-pill" onclick={() => (editing = '')}>Cancel</button>
						</div>
					</div>
				{/if}
			</section>
		</div>

		<section class="box">
			<b>How it switches</b>
			<dl class="facts">
				<dt>Swing</dt>
				<dd>{swing}</dd>
				<dt>Once on, stays on at least</dt>
				<dd>{minutes(a.min_on_secs)}</dd>
				<dt>Once off, stays off at least</dt>
				<dd>
					{minutes(a.min_off_secs)}{kindOf(dev) === 'ac' && num(a.min_off_secs)
						? ' — protects the compressor'
						: ''}
				</dd>
				{#if num(a.max_run_secs)}
					<dt>Longest run</dt>
					<dd>{minutes(a.max_run_secs)}, then off and an alert</dd>
				{/if}
				<dt>A sensor goes quiet after</dt>
				<dd>
					{minutes(a.sensor_timeout_secs ?? 7200)}, then {a.on_stale === 'hold'
						? `${k.the} is left as it is`
						: `${k.the} is turned off`}
				</dd>
				<dt>Switched by hand</dt>
				<dd>
					{num(a.override_secs)
						? `left alone for ${minutes(a.override_secs)}`
						: 'taken over again at once'}
				</dd>
				{#if kindOf(dev) === 'heater'}
					<dt>When the plugin stops</dt>
					<dd>turned off</dd>
				{/if}
			</dl>
			{#if canRun}<button class="link" onclick={() => edit('guards')}>Change ›</button>{/if}
			{#if editing === 'guards'}
				<div class="editor grid">
					<label class="hc-field"
						><span class="hc-label">Swing</span>
						<select bind:value={pickSwing}>
							{#each SWINGS as s (s)}<option value={s}>± {diffText(s / 2, units)}</option>{/each}
						</select></label
					>
					<label class="hc-field"
						><span class="hc-label">Once on, at least</span>
						<select bind:value={pickOn}>
							{#each GUARDS as g (g)}<option value={g}>{g ? minutes(g) : 'no guard'}</option>{/each}
						</select></label
					>
					<label class="hc-field"
						><span class="hc-label">Once off, at least</span>
						<select bind:value={pickOff}>
							{#each GUARDS as g (g)}<option value={g}>{g ? minutes(g) : 'no guard'}</option>{/each}
						</select></label
					>
					<span class="hc-quiet small"
						>The longest run, the quiet time and the hand-flip time are in Settings.</span
					>
					<div class="hc-row">
						<button class="hc-pill go" onclick={save}>Save</button>
						<button class="hc-pill" onclick={() => (editing = '')}>Cancel</button>
					</div>
				</div>
			{/if}
		</section>
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 16px;
		container-type: inline-size;
	}
	.panel.off {
		opacity: 0.6;
	}
	p {
		margin: 0;
	}
	.kind {
		font-size: 14px;
		margin-top: -8px;
	}
	.top,
	.two {
		display: flex;
		flex-wrap: wrap;
		gap: 16px;
		align-items: stretch;
	}
	.top > *,
	.two > * {
		flex: 1 1 300px;
		min-width: 0;
	}
	.side {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.box {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		box-sizing: border-box;
		min-width: 0;
	}
	.warnbox {
		background: light-dark(#fbf3e4, #2f2818);
		border-color: light-dark(#efdcbb, #4a3d22);
	}
	.dial {
		align-items: center;
		gap: 14px;
		padding: 22px;
		border-radius: 18px;
	}
	.ring {
		width: 240px;
		max-width: 100%;
		aspect-ratio: 1;
		border-radius: 50%;
		border: 10px solid var(--card-line);
		box-sizing: border-box;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 4px;
		text-align: center;
		padding: 0 18px;
	}
	.ring.cool {
		border-color: light-dark(#1f5a85, #8cc4ec);
	}
	.ring.heat {
		border-color: light-dark(#b5530c, #f0a46b);
	}
	.ring .hc-quiet {
		font-size: 14px;
	}
	.ring .small,
	.small {
		font-size: 13px;
	}
	.room {
		font-size: 64px;
		font-weight: 600;
		line-height: 1;
		font-family: var(--font);
	}
	.target {
		display: flex;
		align-items: center;
		gap: 14px;
	}
	.round {
		width: 52px;
		height: 52px;
		border-radius: 50%;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font-size: 24px;
	}
	.tv {
		display: flex;
		flex-direction: column;
		align-items: center;
		min-width: 120px;
	}
	.tv b {
		font-size: 30px;
		font-weight: 600;
		font-family: var(--font);
	}
	.tv .small {
		font-size: 12px;
	}
	.seg {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px;
		padding: 4px;
		border-radius: 12px;
		background: var(--nav-on, var(--line));
		width: 100%;
		box-sizing: border-box;
	}
	.seg button {
		min-height: 44px;
		border: none;
		background: none;
		border-radius: 9px;
		font-weight: 600;
		font-size: 15px;
		color: var(--ink-muted);
	}
	.seg button.on {
		background: var(--surface);
		color: var(--ink);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
	}
	.note {
		font-size: 12px;
	}
	.cap {
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--ink-muted);
	}
	.now {
		gap: 6px;
	}
	.title {
		font-size: 20px;
		font-weight: 600;
	}
	.title.cool {
		color: light-dark(#1f5a85, #8cc4ec);
	}
	.title.heat {
		color: light-dark(#a3470a, #f0a46b);
	}
	.title.warn {
		color: light-dark(#8a3a05, #f0b27a);
	}
	.title.quiet {
		color: var(--ink-muted);
	}
	.why {
		font-size: 14px;
		color: color-mix(in srgb, var(--ink) 80%, var(--ink-muted));
	}
	.now .hc-pill,
	.box .hc-pill {
		align-self: flex-start;
		min-height: 40px;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.chip {
		min-height: 40px;
		padding: 0 14px;
		border-radius: 999px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font-size: 14px;
	}
	.chip.on {
		border: 2px solid var(--accent);
		background: var(--lit-bg);
		color: var(--lit-ink, var(--ink));
		font-weight: 600;
	}
	.said {
		font-size: 13px;
	}
	.head {
		display: flex;
		align-items: baseline;
		gap: 10px;
		flex-wrap: wrap;
	}
	.head b {
		flex: 1;
	}
	.head .hc-quiet {
		font-size: 14px;
	}
	b {
		font-weight: 600;
	}
	.line {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 14px;
		min-width: 0;
	}
	.grow {
		flex: 1;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.age {
		font-size: 13px;
		white-space: nowrap;
	}
	.line .hc-num,
	.pick .hc-num {
		font-size: 13px;
	}
	.link {
		align-self: flex-start;
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
		font-size: 14px;
		min-height: 32px;
	}
	.hc-row {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.facts {
		display: grid;
		grid-template-columns: 220px minmax(0, 1fr);
		row-gap: 8px;
		column-gap: 12px;
		margin: 0;
		font-size: 14px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
	}
	.editor {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding-top: 8px;
		border-top: 1px solid var(--card-line);
	}
	.editor.grid .hc-field {
		max-width: 260px;
	}
	.pick {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 44px;
		padding: 0 12px;
		border-radius: 11px;
		border: 1px solid var(--line);
		font-size: 14px;
	}
	.pick input {
		width: 18px;
		height: 18px;
	}
	.hc-field select {
		min-height: 44px;
		border-radius: var(--radius-chip, 11px);
		padding: 0 12px;
	}
	.err {
		color: var(--alert);
		font-size: 14px;
	}
	button:disabled {
		opacity: 0.45;
		cursor: default;
	}
	@container (max-width: 520px) {
		.facts {
			grid-template-columns: minmax(0, 1fr);
			row-gap: 2px;
		}
		dd {
			margin-bottom: 6px;
		}
		.room {
			font-size: 54px;
		}
	}
</style>
