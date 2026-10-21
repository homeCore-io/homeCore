<svelte:options
	customElement={{
		tag: 'hc-yolink-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: lock, unlock, auto-lock, beep volume. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(_h, _ids, value) {
					return sentence(value);
				}
			}
	}}
/>

<!-- The automation editor's step on a YoLink lock: Lock, Unlock (marked:
     nobody may be there), Change auto-lock (`{"auto_lock_secs": N}`) or Beep
     volume (`{"sound_level": N}`). -->
<script module>
	import { kindOf, sentence } from './lib/step.js';
</script>

<script>
	import { build, stateOf, stored } from './lib/step.js';
	import { adopt, AUTO_LOCK, num, SOUND } from './lib/yolink.js';

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
	const first = $derived(devices.find((d) => d.device_id === ids[0]));
	// A v1 lock has no settings to change.
	const settable = $derived(first?.attributes?.auto_lock_secs !== undefined || first?.attributes?.sound_level !== undefined);
	const opts = $derived({ secs: num(v.auto_lock_secs) ?? 30, level: num(v.sound_level) ?? 1 });

	function set(nextIds, k, o) {
		const state = stateOf(k, o);
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const mode = (k) => k !== kind && set(ids, k, opts);

	const names = $derived(ids.map((id) => devices.find((d) => d.device_id === id)).filter(Boolean).map(label));
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, opts));
		const who = names.length <= 2 ? names.join(' and ') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s ? s.replace('{devices}', who || 'the lock').replace(/^./, (c) => c.toUpperCase()) : '';
	});
	const MODES = $derived([
		['lock', 'Lock'],
		['unlock', 'Unlock'],
		...(settable || kind === 'auto' || kind === 'beep'
			? [
					['auto', 'Change auto-lock'],
					['beep', 'Beep volume']
				]
			: [])
	]);
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
			<select value={ids[0]} onchange={(e) => set([e.currentTarget.value], kind, opts)}>
				{#each locks as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			</select></label
		>
	{:else}
		<p class="hc-quiet">On {names.join(', ')}.</p>
	{/if}

	{#if kind === 'auto'}
		<label class="hc-field"
			><span class="hc-label">Lock again after</span>
			<select value={String(opts.secs)} onchange={(e) => set(ids, 'auto', { ...opts, secs: Number(e.currentTarget.value) })}>
				{#each AUTO_LOCK as [s, words] (s)}<option value={String(s)}>{words}</option>{/each}
			</select>
			<span class="hc-quiet small">E.g. a longer delay while the garden is being worked on, back to 10 s at night.</span></label
		>
	{/if}

	{#if kind === 'beep'}
		<label class="hc-field"
			><span class="hc-label">Beep volume</span>
			<select value={String(opts.level)} onchange={(e) => set(ids, 'beep', { ...opts, level: Number(e.currentTarget.value) })}>
				{#each SOUND as [l, words] (l)}<option value={String(l)}>{words}</option>{/each}
			</select></label
		>
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
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px;
		padding: 4px;
		border-radius: 12px;
		background: var(--nav-on, var(--line));
	}
	.seg button {
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
	.hc-field .hc-label {
		font-size: 13px;
		font-weight: 600;
		color: var(--ink);
	}
	.small {
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
