<svelte:options
	customElement={{
		tag: 'hc-thermostat-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: a target, a mode, a hold, back to usual. */
				static accepts(value) {
					return kindOfStep(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					return sentence(value, h?.units ? { temperature: h.units.temperature } : undefined);
				}
			}
	}}
/>

<!-- The automation editor's step on a thermostat: set the target, set the
     mode (only the ones its kind has), hold a different target for a while,
     or go back to the usual target. Kept in °C, said in the person's unit. -->
<script module>
	import { kindOfStep, sentence } from './lib/step.js';
</script>

<script>
	import { build, LENGTHS, optsOf, stateOf, stored } from './lib/step.js';
	import {
		adopt,
		attrs,
		kindWords,
		MODE_NAMES,
		nextTarget,
		targetText,
		targetsAround,
		thermostatsOf
	} from './lib/thermo.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const all = $derived(thermostatsOf(devices, label));

	let units = $state({ temperature: 'C' });
	$effect(() => {
		if (!hc?.units) return;
		units = { temperature: hc.units.temperature };
		return hc.units.subscribe?.((u) => (units = u));
	});

	// The step being edited, kept here so quick changes build on each other.
	let draft = $state(null);
	const v = $derived(draft?.state ?? value ?? {});
	const ids = $derived(draft?.ids ?? deviceIds);
	const kind = $derived(kindOfStep(v) ?? 'target');
	const dev = $derived(devices.find((d) => d.device_id === ids[0]));
	const k = $derived(kindWords(dev));
	const usual = $derived(attrs(dev).setpoint ?? 21);
	const opts = $derived(
		optsOf(v, {
			target: usual,
			mode: k.modes[0],
			holdAt: Math.round((usual - 1) * 2) / 2
		})
	);

	function set(nextIds, kd, o) {
		const state = stateOf(kd, o);
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const change = (patch) => set(ids, kind, { ...opts, ...patch });
	const mode = (kd) => kd !== kind && set(ids, kd, opts);
	// A mode its kind hasn't got can't carry over to another thermostat.
	const pick = (id) => {
		const d = devices.find((x) => x.device_id === id);
		const modes = kindWords(d).modes;
		set([id], kind, { ...opts, mode: modes.includes(opts.mode) ? opts.mode : modes[0] });
	};

	// A new, empty step says what it will do from the start.
	$effect(() => {
		if (!draft && hc && dev && !Object.keys(value ?? {}).length)
			set(ids, 'target', { ...opts, target: usual });
	});

	const names = $derived(
		ids
			.map((id) => devices.find((d) => d.device_id === id))
			.filter(Boolean)
			.map(label)
	);
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, opts), units);
		const who =
			names.length <= 2
				? names.join(' and ')
				: `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s
			? s.replace('{devices}', who || 'the thermostat').replace(/^./, (c) => c.toUpperCase())
			: '';
	});
	const MODES = [
		['target', 'Set the target'],
		['mode', 'Set the mode'],
		['hold', 'Hold for a while'],
		['back', 'Back to usual']
	];
	const holdTargets = $derived(targetsAround(opts.holdAt, units, 8, 8));
</script>

<div class="step">
	{#if said}<p class="said">{said}</p>{/if}

	<span class="seg two" role="group" aria-label="What to do">
		{#each MODES as [kd, name] (kd)}
			<button class:on={kind === kd} aria-pressed={kind === kd} onclick={() => mode(kd)}
				>{name}</button
			>
		{/each}
	</span>

	{#if kind === 'target'}
		<div class="target">
			<button
				class="round"
				aria-label="Lower"
				onclick={() => change({ target: nextTarget(opts.target, -1, units) })}>−</button
			>
			<span class="tv hc-num">{targetText(opts.target, units)}</span>
			<button
				class="round"
				aria-label="Raise"
				onclick={() => change({ target: nextTarget(opts.target, 1, units) })}>+</button
			>
		</div>
	{:else if kind === 'mode'}
		<span class="seg" role="group" aria-label="Mode">
			{#each k.modes as m (m)}
				<button
					class:on={opts.mode === m}
					aria-pressed={opts.mode === m}
					onclick={() => change({ mode: m })}>{MODE_NAMES[m]}</button
				>
			{/each}
		</span>
		<span class="hc-quiet"
			>Only what this thermostat can do — {k.a}
			{k.modes.includes('heat') ? 'has no Cool' : 'has no Heat'}.</span
		>
	{:else if kind === 'hold'}
		<div class="pair">
			<label class="hc-field"
				><span class="hc-label">Keep it at</span>
				<select
					value={String(opts.holdAt)}
					onchange={(e) => change({ holdAt: Number(e.currentTarget.value) })}
				>
					{#each holdTargets as t (t.value)}<option value={String(t.value)}>{t.text}</option>{/each}
				</select></label
			>
			<label class="hc-field"
				><span class="hc-label">For</span>
				<select
					value={String(opts.forSecs)}
					onchange={(e) => change({ forSecs: Number(e.currentTarget.value) })}
				>
					{#if !LENGTHS.some(([s]) => s === opts.forSecs)}
						<option value={String(opts.forSecs)}>{Math.round(opts.forSecs / 60)} minutes</option>
					{/if}
					{#each LENGTHS as [s, w] (s)}<option value={String(s)}>{w}</option>{/each}
				</select></label
			>
		</div>
		<span class="hc-quiet">Then it goes back to its usual target by itself.</span>
	{:else}
		<span class="quiet-text"
			>Ends a hold, or a hand-flip override, and goes back to its usual target now.</span
		>
	{/if}

	{#if ids.length === 1}
		<label class="hc-field"
			><span class="hc-label">Thermostat</span>
			<select value={ids[0]} onchange={(e) => pick(e.currentTarget.value)}>
				{#each all as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			</select></label
		>
	{:else}
		<p class="hc-quiet">On {names.join(', ')}.</p>
	{/if}

	<div class="stored">
		<span class="hc-quiet">Stored as (°C)</span>
		<code>{stored(stateOf(kind, opts))}</code>
	</div>
</div>

<style>
	.step {
		display: flex;
		flex-direction: column;
		gap: 18px;
	}
	.step > * {
		max-width: 560px;
	}
	.said {
		margin: 0;
		padding: 12px 14px;
		border-radius: 12px;
		background: var(--lit-bg);
		color: var(--lit-ink, var(--ink));
		font-size: 16px;
		font-weight: 600;
	}
	.seg {
		display: flex;
		gap: 6px;
		padding: 4px;
		border-radius: 12px;
		background: var(--nav-on, var(--line));
	}
	.seg.two {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
	}
	.seg button {
		flex: 1;
		min-height: 40px;
		border: none;
		background: none;
		border-radius: 9px;
		font-weight: 600;
		font-size: 14px;
		color: var(--ink-muted);
	}
	.seg button.on {
		background: var(--surface);
		color: var(--ink);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
	}
	.target {
		display: flex;
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
	.pair {
		display: flex;
		gap: 10px;
	}
	.pair > * {
		flex: 1;
	}
	.hc-field select {
		min-height: 44px;
		border-radius: var(--radius-chip, 11px);
		padding: 0 12px;
		box-sizing: border-box;
	}
	.hc-field .hc-label {
		font-size: 13px;
		font-weight: 600;
		color: var(--ink);
	}
	.hc-quiet {
		font-size: 13px;
	}
	.quiet-text {
		font-size: 14px;
		color: color-mix(in srgb, var(--ink) 80%, var(--ink-muted));
	}
	.stored {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.stored .hc-quiet {
		font-size: 12px;
	}
	code {
		font-family: var(--font-num, monospace);
		font-size: 13px;
		padding: 10px 12px;
		border-radius: 10px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 80%, var(--surface));
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	p {
		margin: 0;
	}
</style>
