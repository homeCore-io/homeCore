<svelte:options
	customElement={{
		tag: 'hc-roku-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: a channel, an input, power, a button. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					const d = h.devices.get(ids?.[0]);
					return sentence(value, (id) => nameIn(d, id));
				}
			}
	}}
/>

<!-- The automation editor's step on a Roku: open a channel (picked by its
     icon, from that Roku's own channels), switch input, turn it on or off, or
     press a button a number of times. -->
<script module>
	import { kindOf, sentence } from './lib/step.js';

	/** A channel's or input's name on this Roku, from its id. */
	function nameIn(d, id) {
		const a = d?.attributes ?? {};
		const all = [
			...(Array.isArray(a.available_apps) ? a.available_apps : []),
			...(Array.isArray(a.available_inputs) ? a.available_inputs : [])
		];
		return all.find((x) => x?.id === id)?.name ?? id;
	}
</script>

<script>
	import Tile from './lib/Tile.svelte';
	import { build, buttonsFor, stateOf, stored } from './lib/step.js';
	import {
		adopt,
		appsOf,
		az,
		canFindRemote,
		inputsOf,
		isTv,
		mostUsed,
		rokusOf,
		search
	} from './lib/roku.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const rokus = $derived(rokusOf(devices, label));

	// The step being edited, kept here so quick changes build on each other.
	let draft = $state(null);
	const v = $derived(draft?.state ?? value ?? {});
	const ids = $derived(draft?.ids ?? deviceIds);
	const kind = $derived(kindOf(v) ?? 'app');
	const dev = $derived(devices.find((d) => d.device_id === ids[0]));
	const apps = $derived(dev ? appsOf(dev) : []);
	const inputs = $derived(dev ? inputsOf(dev) : []);
	const opts = $derived({
		app: v.app,
		input: v.source ?? inputs[0]?.id,
		on: v.on !== false,
		key: v.key ?? 'VolumeUp',
		times: Number(v.count ?? v.repeat ?? 1) || 1
	});

	function set(nextIds, k, o) {
		const state = stateOf(k, o);
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const change = (patch) => set(ids, kind, { ...opts, ...patch });
	const mode = (k) => k !== kind && set(ids, k, opts);
	const pickRoku = (id) => {
		const d = devices.find((x) => x.device_id === id);
		// A channel or input this Roku has not got can't carry over.
		const has = (x) => appsOf(d).some((a) => a.id === x) || inputsOf(d).some((a) => a.id === x);
		set([id], kind, {
			...opts,
			app: has(opts.app) ? opts.app : mostUsed(d, 1)[0]?.id,
			input: has(opts.input) ? opts.input : inputsOf(d)[0]?.id
		});
	};

	// A new step says what it will do from the start: the most used channel.
	$effect(() => {
		if (!draft && hc && kind === 'app' && !value?.app && apps.length)
			set(ids, 'app', { ...opts, app: mostUsed(dev, 1)[0]?.id });
	});

	let all = $state(false);
	let q = $state('');
	const shown = $derived(
		all
			? search(az(apps), q)
			: mostUsed(dev, 8).concat(
					// The one picked shows even when it isn't among the eight.
					apps.filter((a) => a.id === opts.app && !mostUsed(dev, 8).includes(a))
				)
	);

	const names = $derived(
		ids
			.map((id) => devices.find((d) => d.device_id === id))
			.filter(Boolean)
			.map(label)
	);
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, opts), (id) => {
			const all = [...apps, ...inputs];
			return all.find((x) => x.id === id)?.name ?? id;
		});
		const who =
			names.length <= 2
				? names.join(' and ')
				: `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s ? s.replace('{devices}', who || 'the Roku').replace(/^./, (c) => c.toUpperCase()) : '';
	});
	const MODES = [
		['app', 'Open a channel'],
		['input', 'Switch input'],
		['power', 'Turn on or off'],
		['key', 'Press a button']
	];
	const buttons = $derived(buttonsFor({ tv: isTv(dev), findRemote: canFindRemote(dev) }));
</script>

<div class="step">
	{#if said}<p class="said">{said}</p>{/if}

	<span class="seg" role="group" aria-label="What to do">
		{#each MODES as [k, name] (k)}
			<button class:on={kind === k} aria-pressed={kind === k} onclick={() => mode(k)}>{name}</button
			>
		{/each}
	</span>

	{#if ids.length === 1}
		<label class="hc-field"
			><span class="hc-label">Roku</span>
			<select value={ids[0]} onchange={(e) => pickRoku(e.currentTarget.value)}>
				{#each rokus as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			</select></label
		>
	{:else}
		<p class="hc-quiet">On {names.join(', ')}.</p>
	{/if}

	{#if kind === 'app'}
		<div class="part">
			<span class="sub">Channel</span>
			{#if all}
				<label class="hc-field find"
					><span class="hc-label">Find a channel</span>
					<input type="search" placeholder="e.g. PBS" bind:value={q} /></label
				>
			{/if}
			{#if apps.length}
				<div class="chans" class:all role="radiogroup" aria-label="Channel">
					{#each shown as app (app.id)}
						<button
							class="chan"
							class:on={opts.app === app.id}
							role="radio"
							aria-checked={opts.app === app.id}
							aria-label={app.name}
							onclick={() => change({ app: app.id })}
						>
							<Tile {hc} {app} lit={opts.app === app.id} />
							<span class="name">{app.name}</span>
						</button>
					{/each}
				</div>
				<button class="link" onclick={() => ((all = !all), (q = ''))}
					>{all ? 'Most used ›' : `All ${apps.length} channels ›`}</button
				>
			{:else}
				<p class="hc-quiet">This Roku's channels are read the next time it answers.</p>
			{/if}
			<span class="hc-quiet"
				>From this Roku's own channels. The TV is turned on first if it's off.</span
			>
		</div>
	{:else if kind === 'input'}
		{#if inputs.length}
			<label class="hc-field"
				><span class="hc-label">Input</span>
				<select value={opts.input} onchange={(e) => change({ input: e.currentTarget.value })}>
					{#each inputs as i (i.id)}<option value={i.id}>{i.name}</option>{/each}
				</select></label
			>
		{:else}
			<p class="hc-quiet">
				{dev && !isTv(dev) ? `${label(dev)} has no inputs to switch.` : 'No inputs read yet.'}
			</p>
		{/if}
	{:else if kind === 'power'}
		<span class="seg" role="group" aria-label="Power">
			<button class:on={opts.on} aria-pressed={opts.on} onclick={() => change({ on: true })}
				>Turn on</button
			>
			<button class:on={!opts.on} aria-pressed={!opts.on} onclick={() => change({ on: false })}
				>Turn off</button
			>
		</span>
	{:else}
		<div class="keyrow">
			<label class="hc-field grow"
				><span class="hc-label">Button</span>
				<select value={opts.key} onchange={(e) => change({ key: e.currentTarget.value })}>
					{#each buttons as [k, name] (k)}<option value={k}>{name}</option>{/each}
				</select></label
			>
			<label class="hc-field times"
				><span class="hc-label">Times</span>
				<input
					type="number"
					min="1"
					max="20"
					value={opts.times}
					onchange={(e) => change({ times: Number(e.currentTarget.value) })}
				/></label
			>
		</div>
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
	}
	.seg button.on {
		background: var(--surface);
		color: var(--ink);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
	}
	.hc-field select,
	.hc-field input {
		min-height: 44px;
		border-radius: var(--radius-chip, 11px);
		padding: 0 12px;
		box-sizing: border-box;
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
		gap: 8px;
	}
	.part > .hc-quiet,
	.hc-quiet {
		font-size: 13px;
	}
	.chans {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 8px;
	}
	.chans.all {
		max-height: 340px;
		overflow-y: auto;
		padding: 4px 4px 4px 2px;
	}
	.chan {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 0;
		border: none;
		background: none;
		min-width: 0;
	}
	.name {
		font-size: 11px;
		color: var(--ink-muted);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		text-align: center;
	}
	.chan.on .name {
		color: var(--ink);
		font-weight: 600;
	}
	.link {
		align-self: flex-start;
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
		font-size: 13px;
	}
	.keyrow {
		display: flex;
		gap: 10px;
	}
	.grow {
		flex: 1;
	}
	.times {
		width: 110px;
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
