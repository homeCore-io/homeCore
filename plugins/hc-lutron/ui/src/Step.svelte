<svelte:options
	customElement={{
		tag: 'hc-lutron-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: press a button, or set its light. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					return sentence(value, buttonsOf(h.devices.get(ids?.[0])));
				}
			}
	}}
/>

<!-- The automation editor's step on a keypad (or visor receiver): press one
     of its buttons, or set a button's light — picked on the keypad drawn as
     it is on the wall. -->
<script module>
	import { kindOf, sentence } from './lib/step.js';
	import { buttonsOf } from './lib/lutron.js';
</script>

<script>
	import Face from './lib/Face.svelte';
	import { build, stateOf } from './lib/step.js';
	import { adopt, isRocker, LED_STATES, ledButtonsIn, num, schemaOf } from './lib/lutron.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const pads = $derived(devices.filter((d) => d.device_type === 'keypad' || d.device_type === 'vcrx'));

	// The step being edited, kept here so quick changes build on each other.
	let draft = $state(null);
	const v = $derived(draft?.state ?? value ?? {});
	const ids = $derived(draft?.ids ?? deviceIds);
	const first = $derived(devices.find((d) => d.device_id === ids[0]));
	const buttons = $derived(buttonsOf(first));
	const kind = $derived(kindOf(v) ?? 'press');

	let lit = $state([]);
	$effect(() => {
		const id = ids[0];
		if (hc && id) void schemaOf(hc, id).then((s) => (lit = ledButtonsIn(s)));
	});
	const hasLed = (n) => lit.includes(n) || first?.attributes?.[`led_${n}`] !== undefined;
	const led = (n) => (hasLed(n) ? 0 : undefined);

	const button = $derived(num(v.button) ?? (kind === 'led' ? buttons.find((b) => hasLed(b.number)) : buttons.find((b) => !isRocker(b.number)))?.number);
	const ledState = $derived(num(v.state) ?? 1);

	function set(nextIds, state) {
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const mode = (k) => {
		if (k === kind) return;
		const b = k === 'led' && !hasLed(button) ? buttons.find((x) => hasLed(x.number))?.number : button;
		set(ids, stateOf(k, b, ledState));
	};
	const pick = (n) => {
		if (kind === 'led' && !hasLed(n)) return;
		set(ids, stateOf(kind, n, ledState));
	};
	// Another keypad: its first button that can do this step.
	const pickPad = (id) => {
		const d = devices.find((x) => x.device_id === id);
		const b = buttonsOf(d).find((x) => (kind === 'led' ? d.attributes[`led_${x.number}`] !== undefined : !isRocker(x.number)));
		set([id], stateOf(kind, b?.number, ledState));
	};

	// A new step says what it will do from the start.
	$effect(() => {
		if (!draft && hc && button !== undefined && (num(value?.button) === undefined || !value?.action)) set(ids, stateOf(kind, button, ledState));
	});

	const names = $derived(ids.map((id) => devices.find((d) => d.device_id === id)).filter(Boolean).map((d) => hc.devices.label(d)));
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, button, ledState), buttons);
		const who = names.length <= 2 ? names.join(' and ') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s ? s.replace('{devices}', who || 'the keypad').replace(/^./, (c) => c.toUpperCase()) : '';
	});
	const json = $derived(JSON.stringify(stateOf(kind, button, ledState)).replaceAll(':', ': ').replaceAll(',', ', '));
</script>

<div class="step">
	{#if said}<p class="said">{said}</p>{/if}

	<span class="seg" role="group" aria-label="What to do">
		<button class:on={kind === 'press'} aria-pressed={kind === 'press'} onclick={() => mode('press')}>Press a button</button>
		<button class:on={kind === 'led'} aria-pressed={kind === 'led'} onclick={() => mode('led')} disabled={!lit.length && !buttons.some((b) => hasLed(b.number))}
			>Set a light</button
		>
	</span>

	{#if ids.length === 1}
		<label class="hc-field"
			><span class="hc-label">Keypad</span>
			<select value={ids[0]} onchange={(e) => pickPad(e.currentTarget.value)}>
				{#each pads as d (d.device_id)}
					<option value={d.device_id}>{hc.devices.label(d)}{d.room ? ` — ${d.room}` : ''}</option>
				{/each}
			</select></label
		>
	{/if}

	<div class="body">
		<Face {buttons} {led} lights={false} selected={button} onpick={pick} label="Pick the button" />
		<div class="side">
			<span class="hc-quiet">Pick the button on the keypad.</span>
			{#if kind === 'led'}
				<span class="sub">Set its light to</span>
				<div class="choices" role="radiogroup" aria-label="Its light">
					{#each LED_STATES as c (c.value)}
						<button class="choice" class:on={ledState === c.value} role="radio" aria-checked={ledState === c.value} onclick={() => set(ids, stateOf('led', button, c.value))}
							>{c.name}</button
						>
					{/each}
				</div>
			{/if}
		</div>
	</div>

	<div class="stored">
		<span class="hc-quiet">Stored as</span>
		<code>{json}</code>
	</div>
</div>

<style>
	.step {
		display: flex;
		flex-direction: column;
		gap: 16px;
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
	.seg button {
		flex: 1;
		min-height: 40px;
		border: none;
		background: none;
		border-radius: 9px;
		font-weight: 600;
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
	.hc-field .hc-label {
		font-size: 13px;
		color: var(--ink);
	}
	.body {
		display: flex;
		gap: 20px;
		align-items: flex-start;
	}
	.side {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
	}
	.sub {
		font-size: 13px;
		font-weight: 600;
	}
	.choices {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px;
	}
	.choice {
		min-height: 40px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 14px;
	}
	.choice.on {
		background: color-mix(in srgb, var(--track-on) 10%, var(--surface));
		border-color: color-mix(in srgb, var(--track-on) 60%, var(--surface));
		font-weight: 600;
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
