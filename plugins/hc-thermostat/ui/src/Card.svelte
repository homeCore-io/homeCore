<svelte:options
	customElement={{
		tag: 'hc-thermostat-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A thermostat's card on Home and the rooms. Standard: its name, what it is
     doing ("Cooling to 68°"), the room's temperature, the target with − and +,
     and its mode. Large adds today's chart and run, and one-tap "66° for 2 h"
     and "Turn off". Compact: the name, the temperature and the line. When it
     needs someone — no reading, the switch not answering, a hand flip being
     left alone — the card says so instead, outlined. Tap it for the panel. -->
<script>
	import Chart from './lib/Chart.svelte';
	import {
		activeMode,
		adopt,
		attrs,
		cardLine,
		holdChips,
		house,
		kindWords,
		MODE_NAMES,
		needsLine,
		needsYou,
		nextTarget,
		nowOf,
		shortName,
		targetNow,
		targetText,
		tempText,
		todayLine
	} from './lib/thermo.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(attrs(dev));
	const k = $derived(kindWords(dev));
	const name = $derived(dev ? shortName(hc.devices.label(dev)) : '');

	let units = $state({ temperature: 'C' });
	$effect(() => {
		if (!hc?.units) return;
		units = { temperature: hc.units.temperature };
		return hc.units.subscribe?.((u) => (units = u));
	});
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 15000);
		return () => clearInterval(t);
	});
	let names = $state({});
	$effect(() => {
		if (!hc || !needsYou(dev)) return;
		let gone = false;
		house(hc).then((h) => {
			if (!gone) names = h.names;
		});
		return () => (gone = true);
	});

	const opts = $derived({ units, names, now });
	const needs = $derived(!!dev && needsYou(dev));
	const tone = $derived(dev ? nowOf(dev, opts).tone : 'plain');
	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);

	let error = $state('');
	async function send(body) {
		const r = await hc.command(deviceId, body);
		error = r.ok ? '' : r.error;
	}
	let want = $state(null);
	let timer;
	const shownTarget = $derived(want ?? targetNow(dev));
	function nudge(dir) {
		want = nextTarget(shownTarget, dir, units);
		clearTimeout(timer);
		timer = setTimeout(async () => {
			const v = want;
			const left = Math.round((Date.parse(a.hold_until) - Date.now()) / 1000);
			await send(
				a.hold_setpoint != null && left > 60
					? { action: 'hold', setpoint: v, for_secs: left }
					: { action: 'set_setpoint', value: v }
			);
		}, 700);
	}
	$effect(() => {
		if (want !== null && Math.abs((targetNow(dev) ?? NaN) - want) < 0.01) want = null;
	});
	const twoHours = $derived(dev ? holdChips(dev, units, now)[1] : null);
	const open = () => dev && hc.ui.openDevice(dev.device_id);
</script>

{#if dev}
	<div class="hc-tile {form}" class:needs class:off={dev.available === false}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		<span class="row">
			<span class="words">
				<span class="hc-tile-name">{name}</span>
				<span class="hc-tile-state one {needs ? 'warn' : tone}">{cardLine(dev, opts)}</span>
			</span>
			<span class="temp hc-num" class:dim={a.temperature == null}
				>{tempText(a.temperature, units)}</span
			>
		</span>

		{#if needs && form !== 'compact'}
			<span class="said">{needsLine(dev, opts)}</span>
		{:else if form === 'standard'}
			<span class="controls">
				<button
					class="sq"
					aria-label="Lower the target"
					disabled={!canRun || a.hvac_mode === 'off'}
					onclick={() => nudge(-1)}>−</button
				>
				<span class="tv hc-num">{targetText(shownTarget, units)}</span>
				<button
					class="sq"
					aria-label="Raise the target"
					disabled={!canRun || a.hvac_mode === 'off'}
					onclick={() => nudge(1)}>+</button
				>
				<label class="mode">
					<span class="sr">Mode</span>
					<select
						value={a.hvac_mode}
						disabled={!canRun}
						onchange={(e) => send({ hvac_mode: e.currentTarget.value })}
					>
						{#each k.modes as m (m)}<option value={m}>{MODE_NAMES[m]}</option>{/each}
					</select>
				</label>
			</span>
		{:else if form === 'large'}
			<span class="chart"><Chart {hc} {dev} {units} height={56} small /></span>
			<span class="today">{todayLine(dev, now).replace(/^ran/, 'Ran')} today</span>
			<span class="controls">
				{#if a.hvac_mode === 'off'}
					<button
						class="wide"
						disabled={!canRun}
						onclick={() => send({ hvac_mode: activeMode(dev) })}
						>Turn on — {MODE_NAMES[activeMode(dev)]}</button
					>
				{:else}
					{#if twoHours}
						<button
							class="wide"
							disabled={!canRun}
							onclick={() =>
								send({ action: 'hold', setpoint: twoHours.setpoint, for_secs: twoHours.for_secs })}
							>{twoHours.name}</button
						>
					{/if}
					<button class="wide" disabled={!canRun} onclick={() => send({ hvac_mode: 'off' })}
						>Turn off</button
					>
				{/if}
			</span>
		{/if}
		{#if error}<span class="err" role="alert">{error}</span>{/if}
	</div>
{/if}

<style>
	.hc-tile {
		position: relative;
		height: 100%;
		gap: 10px;
		padding: 14px;
		box-sizing: border-box;
		container-type: inline-size;
	}
	.hc-tile.compact {
		padding: 10px 12px;
	}
	.hc-tile.needs {
		border: 2px solid #d9822b;
		padding: 13px;
	}
	.hc-tile.compact.needs {
		padding: 9px 11px;
	}
	.off {
		opacity: 0.6;
	}
	.cover {
		position: absolute;
		inset: 0;
		border: none;
		background: none;
		border-radius: inherit;
		z-index: 0;
	}
	.row,
	.controls,
	.said,
	.chart,
	.today,
	.err {
		position: relative;
		z-index: 1;
	}
	.row,
	.said,
	.chart,
	.today {
		pointer-events: none;
	}
	.row {
		display: flex;
		align-items: flex-start;
		gap: 10px;
		min-width: 0;
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.one {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.hc-tile-state.cool {
		color: light-dark(#1f5a85, #8cc4ec);
		font-weight: 600;
	}
	.hc-tile-state.heat {
		color: light-dark(#a3470a, #f0a46b);
		font-weight: 600;
	}
	.hc-tile-state.warn {
		color: light-dark(#8a3a05, #f0b27a);
		font-weight: 600;
	}
	.temp {
		font-size: 30px;
		font-weight: 600;
		line-height: 1;
		font-family: var(--font);
	}
	.compact .temp {
		font-size: 22px;
	}
	.temp.dim {
		color: var(--ink-muted);
	}
	.said {
		font-size: 13px;
		color: color-mix(in srgb, var(--ink) 80%, var(--ink-muted));
	}
	.today {
		font-size: 13px;
		color: color-mix(in srgb, var(--ink) 80%, var(--ink-muted));
	}
	.controls {
		display: flex;
		gap: 8px;
		margin-top: auto;
		align-items: center;
	}
	.sq {
		flex: none;
		min-height: 44px;
		min-width: 44px;
		border-radius: 11px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font-size: 18px;
	}
	.tv {
		flex: 1;
		text-align: center;
		font-weight: 600;
		font-family: var(--font);
		font-size: 16px;
	}
	.mode select {
		min-height: 44px;
		border-radius: 11px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 10px;
		font: inherit;
	}
	.wide {
		flex: 1;
		min-height: 44px;
		border-radius: 11px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font: inherit;
	}
	button:disabled,
	select:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
	.err {
		font-size: 12px;
		color: var(--alert);
	}
	.words .hc-tile-name {
		overflow-wrap: anywhere;
	}
	/* A card one column wide: the name may take two lines, the mode is on
	   the panel. */
	@container (max-width: 300px) {
		.words .hc-tile-name,
		.standard .one,
		.large .one {
			white-space: normal;
		}
		.temp {
			font-size: 24px;
		}
		.mode {
			display: none;
		}
		.controls {
			gap: 6px;
		}
	}
</style>
