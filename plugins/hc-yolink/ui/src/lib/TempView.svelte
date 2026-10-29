<!-- A temperature sensor: its reading, "Tell me when" — kept as homeCore's
     own alert rules, so they ring the bell, show on the Alerts page, clear by
     themselves and can start automations — and how it reports. The sensor's
     own limits (set in the YoLink app) are shown, not changed. -->
<script>
	import { ago, ALERT_RULES_HREF, batteryLow, BATTERY_WORDS, quarters, quietAfter, SIGNAL_WORDS, temp, tempNumber, tempUnit, toCelsius } from './yolink.js';
	import { build, channels, FOR_CHOICES, readRules, rulesFor, ruleName, saveRule } from './tell.js';

	let { hc, dev, row = null, status = null } = $props();
	let alerts = $state();
	$effect(() => hc?.alerts?.subscribe((list) => (alerts = list)));

	const a = $derived(dev?.attributes ?? {});
	const label = $derived(dev ? hc.devices.label(dev) : '');
	let unitsTick = $state(0);
	$effect(() => hc?.units?.subscribe?.(() => unitsTick++));
	const unit = $derived((void unitsTick, tempUnit(hc)));
	const q = $derived(quarters(dev));
	const canEdit = $derived(!!hc?.me.can('automations:write'));
	const off = $derived(dev?.available === false);

	let rules = $state([]);
	let readError = $state('');
	let chans = $state([]);
	async function read() {
		const r = await readRules(hc);
		rules = r.rules;
		readError = r.error;
	}
	const id = $derived(dev?.device_id);
	$effect(() => {
		if (!hc || !id) return;
		void read();
		void channels(hc).then((c) => (chans = c));
	});
	const mine = $derived(rulesFor(rules, dev?.device_id));
	const on = (r) => !!r && r.enabled !== false;

	// The form: in the person's units, saved in °C.
	let warm = $state({ on: false, value: '', for: 15 });
	let cold = $state({ on: false, value: '' });
	let notify = $state('');
	let loadedFor = $state('');
	$effect(() => {
		// Fill the form from the rules once they are read (and again for
		// another device), not while someone is typing.
		const key = `${dev?.device_id}|${mine.above?.id ?? ''}|${mine.below?.id ?? ''}|${unit}`;
		if (key === loadedFor) return;
		loadedFor = key;
		warm = { on: on(mine.above), value: mine.above ? String(tempNumber(hc, mine.above.value)) : '', for: mine.above?.for_minutes ?? 15 };
		cold = { on: on(mine.below), value: mine.below ? String(tempNumber(hc, mine.below.value)) : '' };
		notify = (mine.above ?? mine.below)?.notify ?? '';
	});

	let saving = $state(false);
	let error = $state('');
	let saved = $state('');
	async function save(op) {
		error = '';
		const f = op === 'above' ? warm : cold;
		const was = op === 'above' ? mine.above : mine.below;
		const value = toCelsius(hc, f.value);
		if (f.on && value === undefined) {
			error = `Give a temperature in ${unit}.`;
			return;
		}
		if (!f.on && !was) return;
		saving = true;
		const rule = build({
			was,
			deviceId: dev.device_id,
			deviceName: label.replace(/ (temperature|temp sensor|temperature sensor)$/i, ''),
			op,
			value: value ?? was?.value,
			forMinutes: op === 'above' ? Number(warm.for) : (was?.for_minutes ?? 0),
			notify,
			enabled: f.on,
			label: `${op === 'above' ? 'Warmer' : 'Colder'} than ${f.value}${unit}`
		});
		const r = await saveRule(hc, rule);
		saving = false;
		if (r.error) error = r.error;
		else {
			saved = `Saved as the alert rule “${r.rule.name}”.`;
			setTimeout(() => (saved = ''), 6000);
			await read();
		}
	}
	const saveBoth = async () => {
		if (mine.above || warm.on) await save('above');
		if (mine.below || cold.on) await save('below');
	};
	const ruleNameShown = $derived(mine.above?.name ?? mine.below?.name ?? ruleName(label.replace(/ (temperature|temp sensor|temperature sensor)$/i, ''), 'above'));
	const limits = $derived(a.temp_limit_low !== undefined && a.temp_limit_high !== undefined ? `${(void unitsTick, temp(hc, a.temp_limit_low))} to ${temp(hc, a.temp_limit_high)}` : '');
</script>

<div class="view">
	<div class="hero">
		<span class="big hc-num" class:off>{off ? 'Not reporting' : (void unitsTick, temp(hc, a.temperature))}</span>
		<span class="hc-quiet side"
			>{a.humidity !== undefined ? `${Math.round(a.humidity)} % humidity` : 'A probe — temperature only'}{a.too_warm === true
				? ' · above its own limit'
				: a.too_cold === true
					? ' · below its own limit'
					: ''}</span
		>
	</div>

	<div class="card">
		<span class="t">Tell me when</span>
		{#if readError === 'forbidden'}
			<span class="hc-quiet">You can’t see the house’s alert rules. Someone who can change automations can set these.</span>
		{:else}
			<label class="line"
				><input type="checkbox" bind:checked={warm.on} disabled={!canEdit || saving} onchange={() => save('above')} /><span>It’s warmer than</span><input
					class="n"
					type="number"
					step="0.5"
					bind:value={warm.value}
					disabled={!canEdit || saving}
					onchange={() => warm.on && save('above')}
				/><span>{unit} for</span><select bind:value={warm.for} disabled={!canEdit || saving} onchange={() => warm.on && save('above')}>
					{#each FOR_CHOICES as [m, words] (m)}<option value={m}>{words}</option>{/each}
				</select></label
			>
			<div class="line">
				<span class="pad"></span><span>Tell</span><select bind:value={notify} disabled={!canEdit || saving} onchange={saveBoth}>
					<option value="">No one — just show it</option>
					<option value="all">Every channel</option>
					{#each chans as c (c)}<option value={c}>{c}</option>{/each}
					{#if notify && notify !== 'all' && !chans.includes(notify)}<option value={notify}>{notify}</option>{/if}
				</select>
				<button class="link" onclick={() => hc.ui.navigate(ALERT_RULES_HREF)}>Do something… ›</button>
			</div>
			<label class="line"
				><input type="checkbox" bind:checked={cold.on} disabled={!canEdit || saving} onchange={() => save('below')} /><span>It’s colder than</span><input
					class="n"
					type="number"
					step="0.5"
					placeholder={unit.includes('F') ? 'e.g. 32' : 'e.g. 0'}
					bind:value={cold.value}
					disabled={!canEdit || saving}
					onchange={() => cold.on && save('below')}
				/><span>{unit}</span></label
			>
			{#if error}<span class="err" role="alert">{error}</span>{/if}
			{#if saved}<span class="ok" role="status">{saved}</span>{/if}
			<span class="hc-quiet small"
				>Saved as the alert rule “{ruleNameShown}” (Settings › Alert rules): on the bell and the Alerts page, clearing by itself when it’s back in range.{limits
					? ` The sensor’s own limits in the YoLink app (${limits}) are shown, not changed.`
					: ''}{canEdit ? '' : ' Only someone who can change automations can change these.'}</span
			>
		{/if}
	</div>

	<div class="tiles">
		<div class="tile" class:warn={off}>
			<span class="hc-label">Checked in</span>
			<span class="v" class:bad={off}>{a.last_seen ? ago(a.last_seen) : '—'}</span>
			{#if quietAfter(status)}<span class="hc-quiet small">offline after {quietAfter(status)} quiet</span>{/if}
		</div>
		<div class="tile">
			<span class="hc-label">Signal</span>
			<span class="v">{SIGNAL_WORDS[row?.signal ?? 'unknown']}</span>
			{#if row?.signal_dbm}<span class="hc-quiet small">{row.signal_dbm} dBm</span>{/if}
		</div>
		<div class="tile" class:warn={batteryLow(dev, alerts)}>
			<span class="hc-label">Battery</span>
			<span class="v" class:bad={batteryLow(dev, alerts)}>{q === undefined ? '—' : `${batteryLow(dev, alerts) ? 'Low' : BATTERY_WORDS[q]} · ${q} of 4`}</span>
		</div>
	</div>
</div>

<style>
	.view {
		display: flex;
		flex-direction: column;
		gap: 18px;
	}
	.hero {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 14px;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.big {
		flex: 1;
		font-size: 30px;
		font-weight: 600;
	}
	.big.off {
		color: var(--alert);
		font-family: inherit;
		font-size: 24px;
	}
	.side {
		font-size: 14px;
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.t {
		font-weight: 600;
	}
	.line {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
		min-height: 44px;
	}
	input[type='checkbox'] {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.pad {
		width: 18px;
	}
	.n {
		width: 80px;
		min-height: 40px;
		border-radius: 10px;
		border: 1px solid var(--line);
		padding: 0 10px;
		background: var(--surface);
		color: var(--ink);
	}
	select {
		min-height: 40px;
		border-radius: 10px;
		border: 1px solid var(--line);
		padding: 0 10px;
		background: var(--surface);
		color: var(--ink);
		max-width: 100%;
	}
	.link {
		margin-left: auto;
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
		font-size: 14px;
	}
	.small {
		font-size: 13px;
	}
	.err {
		color: var(--alert);
		font-size: 14px;
	}
	.ok {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 12px;
		border-radius: 12px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		min-width: 0;
	}
	.tile.warn {
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
	}
	.tile .small {
		font-size: 12px;
	}
	.v {
		font-weight: 600;
	}
	.v.bad {
		color: var(--alert);
	}
	@media (max-width: 480px) {
		.tiles {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.link {
			margin-left: 28px;
		}
	}
</style>
