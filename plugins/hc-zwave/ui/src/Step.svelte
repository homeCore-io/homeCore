<svelte:options
	customElement={{
		tag: 'hc-zwave-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: lock, unlock, a code on or off. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					const names = knownNames.get(ids?.[0]) ?? {};
					if (ids?.[0] && !knownNames.has(ids[0]) && kindOf(value) === 'code') {
						// Learn the slots' names for next time; this time says "code 2".
						knownNames.set(ids[0], {});
						void h.query('list_user_codes', { device_id: ids[0] }).then((r) => {
							if (!r.ok) return;
							const n = {};
							for (const s of r.data?.slots ?? []) if (s.name) n[s.slot] = s.name;
							knownNames.set(ids[0], n);
						});
					}
					return sentence(value, names);
				}
			}
	}}
/>

<!-- The automation editor's step on a lock: Lock, Unlock (marked: nobody may
     be there), or turn one of its codes on or off — `{"action":
     "set_code_enabled", "slot": N, "enabled": bool}`. Codes themselves are set
     on the lock's Codes tab, never in an automation. -->
<script module>
	import { kindOf, knownNames, sentence } from './lib/step.js';
</script>

<script>
	import { build, codeWords, stateOf, stored } from './lib/step.js';
	import { adopt, slotNames } from './lib/zwave.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const locks = $derived(devices.filter((d) => d.device_type === 'lock').sort((x, y) => label(x).localeCompare(label(y))));

	let draft = $state(null);
	const v = $derived(draft?.state ?? value ?? {});
	const ids = $derived(draft?.ids ?? deviceIds);
	const kind = $derived(kindOf(v) ?? 'lock');

	// The first lock's code slots, by who they are for.
	let codes = $state({ names: {}, slots: [] });
	$effect(() => {
		const id = ids[0];
		if (!hc || !id) return;
		void slotNames(hc, id).then((r) => {
			codes = r;
			knownNames.set(id, r.names);
		});
	});
	const usable = $derived(codes.slots.filter((s) => s.status === 'in_use' || s.status === 'disabled'));
	const opts = $derived({ slot: v.slot ?? usable[0]?.slot ?? 1, enabled: v.enabled !== false });

	function set(nextIds, kindNow, o) {
		const state = stateOf(kindNow, o);
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const change = (patch) => set(ids, kind, { ...opts, ...patch });
	const mode = (k) => k !== kind && set(ids, k, opts);

	const names = $derived(ids.map((id) => devices.find((d) => d.device_id === id)).filter(Boolean).map(label));
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, opts), codes.names);
		const who = names.length <= 2 ? names.join(' and ') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s ? s.replace('{devices}', who || 'the lock').replace(/^./, (c) => c.toUpperCase()) : '';
	});
	const MODES = [
		['lock', 'Lock'],
		['unlock', 'Unlock'],
		['code', 'Turn a code on or off']
	];
</script>

<div class="step">
	{#if said}<p class="said">{said}</p>{/if}

	<span class="seg" role="group" aria-label="What to do">
		{#each MODES as [k, nm] (k)}
			<button class:on={kind === k} aria-pressed={kind === k} onclick={() => mode(k)}>{nm}</button>
		{/each}
	</span>

	{#if ids.length === 1}
		<label class="hc-field"
			><span class="hc-label">Lock</span>
			<select value={ids[0]} onchange={(e) => set([e.currentTarget.value], kind, kind === 'code' ? { ...opts, slot: undefined } : opts)}>
				{#each locks as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			</select></label
		>
	{:else}
		<p class="hc-quiet">On {names.join(', ')}.</p>
	{/if}

	{#if kind === 'code'}
		<div class="part">
			<span class="sub">Code</span>
			{#if usable.length}
				<div class="radios" role="radiogroup" aria-label="Code">
					{#each usable as s (s.slot)}
						<label class="check"
							><input type="radio" name="slot" checked={opts.slot === s.slot} onchange={() => change({ slot: s.slot })} /><span
								>{s.slot}{codes.names[s.slot] ? ` · ${codes.names[s.slot]}` : ''}{s.status === 'disabled' ? ' (off now)' : ''}</span
							></label
						>
					{/each}
				</div>
			{:else}
				<p class="hc-quiet">The lock has no codes yet. Add them on its Codes tab.</p>
			{/if}
			<span class="seg" role="group" aria-label="On or off">
				<button class:on={opts.enabled} aria-pressed={opts.enabled} onclick={() => change({ enabled: true })}>Turn it on</button>
				<button class:on={!opts.enabled} aria-pressed={!opts.enabled} onclick={() => change({ enabled: false })}>Turn it off</button>
			</span>
			<span class="hc-quiet">Turning a code off keeps it on the lock, so it can be turned back on — e.g. {codeWords(opts.slot, codes.names)} only on Tuesdays. Codes themselves are set on the lock’s Codes tab, never in an automation.</span>
		</div>
	{/if}

	{#if kind === 'unlock'}
		<span class="warn">Unlocking from an automation opens the door with nobody there. It is marked on the automation’s card.</span>
	{/if}

	<div class="stored">
		<span class="hc-quiet">Stored as</span>
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
	.seg button {
		flex: 1;
		min-height: 40px;
		border: none;
		background: none;
		border-radius: 9px;
		font-weight: 600;
		font-size: 14px;
		color: var(--ink-muted);
		line-height: 1.15;
	}
	.seg button.on {
		background: var(--surface);
		color: var(--ink);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
	}
	.hc-field select {
		min-height: 44px;
		border-radius: var(--radius-chip, 11px);
		padding: 0 12px;
	}
	.hc-field .hc-label,
	.sub {
		font-size: 13px;
		font-weight: 600;
		color: var(--ink);
	}
	.part {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.radios {
		display: flex;
		flex-direction: column;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 44px;
		font-size: 15px;
	}
	.check input {
		width: 18px;
		height: 18px;
	}
	.part .hc-quiet {
		font-size: 13px;
	}
	.warn {
		font-size: 13px;
		color: var(--ink-muted);
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
		border-radius: 10px;
		padding: 10px 12px;
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
