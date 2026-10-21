<svelte:options
	customElement={{
		tag: 'hc-caseta-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: run a scene, or press a Pico button. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					const d = h.devices.get(ids?.[0]);
					return sentence(value, buttonsOf(d), d?.device_type === 'pico_remote');
				}
			}
	}}
/>

<!-- The automation editor's step on a Caséta scene or Pico: run a scene the
     bridge keeps, or press a Pico button as if by hand. -->
<script module>
	import { kindOf, sentence } from './lib/step.js';
	import { buttonsOf } from './lib/lutron.js';
</script>

<script>
	import { build, shown, stateOf } from './lib/step.js';
	import { adopt, num } from './lib/lutron.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const scenes = $derived(
		devices
			.filter((d) => d.device_type === 'scene')
			.sort((x, y) => label(x).localeCompare(label(y)))
	);
	const picos = $derived(
		devices
			.filter((d) => d.device_type === 'pico_remote')
			.sort((x, y) => label(x).localeCompare(label(y)))
	);

	// The step being edited, kept here so quick changes build on each other.
	let draft = $state(null);
	const v = $derived(draft?.state ?? value ?? {});
	const ids = $derived(draft?.ids ?? deviceIds);
	const first = $derived(devices.find((d) => d.device_id === ids[0]));
	const isPico = $derived(first?.device_type === 'pico_remote');
	const kind = $derived(isPico ? 'press' : 'scene');
	const buttons = $derived(buttonsOf(first));
	const button = $derived(num(v.button) ?? buttons[0]?.number);

	function set(nextIds, state) {
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const mode = (k) => {
		if (k === kind) return;
		const d = k === 'press' ? picos[0] : scenes[0];
		if (d) set([d.device_id], stateOf(k, buttonsOf(d)[0]?.number));
	};
	const pickScene = (id) => set([id], stateOf('scene'));
	const pickPico = (id) => {
		const d = devices.find((x) => x.device_id === id);
		const b = buttonsOf(d);
		set([id], stateOf('press', b.some((x) => x.number === button) ? button : b[0]?.number));
	};

	// A new step says what it will do from the start.
	$effect(() => {
		if (draft || !hc || !first) return;
		if (kind === 'scene' && v.action !== 'activate') set(ids, stateOf('scene'));
		if (
			kind === 'press' &&
			(v.action !== 'press' || num(v.button) === undefined) &&
			button !== undefined
		)
			set(ids, stateOf('press', button));
	});

	const names = $derived(
		ids
			.map((id) => devices.find((d) => d.device_id === id))
			.filter(Boolean)
			.map(label)
	);
	const who = $derived(
		names.length <= 2 ? names.join(' and ') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`
	);
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, button), buttons, isPico);
		return s ? s.replace('{devices}', who || 'the Pico').replace(/^./, (c) => c.toUpperCase()) : '';
	});
	const stored = $derived(`${shown(stateOf(kind, button))}  on ${who}`);
</script>

<div class="step">
	{#if said}<span class="said">{said}</span>{/if}

	<span class="seg" role="group" aria-label="What to do">
		<button
			class:on={kind === 'scene'}
			aria-pressed={kind === 'scene'}
			disabled={!scenes.length}
			onclick={() => mode('scene')}>Run a bridge scene</button
		>
		<button
			class:on={kind === 'press'}
			aria-pressed={kind === 'press'}
			disabled={!picos.length}
			onclick={() => mode('press')}>Press a Pico button</button
		>
	</span>

	{#if kind === 'scene'}
		<label class="hc-field"
			><span class="hc-label">Scene</span>
			<select value={ids[0]} onchange={(e) => pickScene(e.currentTarget.value)}>
				{#each scenes as s (s.device_id)}
					<option value={s.device_id}>{label(s)}</option>
				{/each}
			</select></label
		>
		<span class="hc-quiet hint">A scene made in the Lutron app, run by the bridge.</span>
	{:else}
		{#if ids.length === 1}
			<label class="hc-field"
				><span class="hc-label">Pico</span>
				<select value={ids[0]} onchange={(e) => pickPico(e.currentTarget.value)}>
					{#each picos as p (p.device_id)}
						<option value={p.device_id}>{label(p)}{p.room ? ` (${p.room})` : ''}</option>
					{/each}
				</select></label
			>
		{/if}
		<div class="field">
			<span class="hc-label">Button</span>
			<div class="pills" role="radiogroup" aria-label="Button">
				{#each buttons as b (b.number)}
					<button
						class="pick"
						class:on={button === b.number}
						role="radio"
						aria-checked={button === b.number}
						onclick={() => set(ids, stateOf('press', b.number))}>{b.name}</button
					>
				{/each}
			</div>
		</div>
		<span class="hc-quiet hint"
			>Does whatever that button is paired to in the Lutron app — as if someone pressed it.</span
		>
	{/if}

	<div class="stored">
		<span class="hc-quiet">Stored as</span>
		<code>{stored}</code>
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
		font-size: 20px;
		font-weight: 600;
	}
	.seg {
		display: flex;
		gap: 6px;
		padding: 4px;
		border-radius: 12px;
		background: var(--nav-on, var(--line));
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
	.seg button:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.hc-field select {
		min-height: 44px;
		border-radius: var(--radius-chip, 11px);
		padding: 0 12px;
	}
	.hc-label {
		font-size: 13px;
		font-weight: 600;
		color: var(--ink);
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.pills {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.pick {
		min-height: 40px;
		padding: 0 14px;
		border-radius: 999px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		font-size: 14px;
	}
	.pick.on {
		border-color: color-mix(in srgb, var(--accent) 55%, var(--surface));
		background: color-mix(in srgb, var(--accent) 9%, var(--surface));
		color: var(--accent);
		font-weight: 600;
	}
	.hint {
		font-size: 13px;
		margin-top: -8px;
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
</style>
