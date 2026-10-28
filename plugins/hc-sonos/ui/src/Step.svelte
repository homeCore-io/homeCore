<svelte:options
	customElement={{
		tag: 'hc-sonos-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: a favourite, a volume, pause, a group. */
				static accepts(value) {
					return kindOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(h, ids, value) {
					return sentence(value, (id) => {
						const d = h.devices.get(id);
						return d ? h.devices.label(d) : id;
					});
				}
			}
	}}
/>

<!-- The automation editor's step on a speaker: play a favourite (and set
     the volume), set the volume, pause, or make its group — one `group` on
     the speaker that leads, the listed ones joining and the rest leaving. -->
<script module>
	import { kindOf, sentence } from './lib/step.js';
</script>

<script>
	import Art from './lib/Art.svelte';
	import { build, stateOf, stored } from './lib/step.js';
	import { adopt, favouritesOf, playlistsOf, num, speakersOf } from './lib/sonos.js';

	let { host: hc, deviceIds = [], value = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const speakers = $derived(speakersOf(devices, label));
	const favs = $derived(favouritesOf(speakers));
	const lists = $derived(playlistsOf(speakers));

	// The step being edited, kept here so quick changes build on each other.
	let draft = $state(null);
	const v = $derived(draft?.state ?? value ?? {});
	const ids = $derived(draft?.ids ?? deviceIds);
	const kind = $derived(kindOf(v) ?? 'fav');
	const opts = $derived({
		favorite: v.favorite ?? favs[0]?.title,
		playlist: v.playlist ?? lists[0]?.title,
		volume: num(v.volume) ?? 20,
		withVolume: kind === 'fav' ? num(v.volume) !== undefined : true,
		members: Array.isArray(v.members) ? v.members : typeof v.members === 'string' && v.members ? [v.members] : []
	});

	function set(nextIds, kindNow, o) {
		const state = stateOf(kindNow, o);
		draft = { ids: nextIds, state };
		me.dispatchEvent(new CustomEvent('change', { detail: { action: build(nextIds, state) } }));
	}
	const change = (patch) => set(ids, kind, { ...opts, ...patch });
	const mode = (k) => k !== kind && set(ids, k, { ...opts, withVolume: k === 'fav' ? kind === 'vol' : opts.withVolume });
	const pickSpeaker = (id) => set([id], kind, { ...opts, members: opts.members.filter((m) => m !== id) });
	const toggleMember = (id) => change({ members: opts.members.includes(id) ? opts.members.filter((m) => m !== id) : [...opts.members, id] });

	// A new step says what it will do from the start.
	$effect(() => {
		if (!draft && hc && kindOf(value) === 'fav' && !value?.favorite && favs[0]) set(ids, 'fav', { ...opts, withVolume: true });
	});

	const names = $derived(ids.map((id) => devices.find((d) => d.device_id === id)).filter(Boolean).map(label));
	const said = $derived.by(() => {
		const s = sentence(stateOf(kind, opts), (id) => {
			const d = devices.find((x) => x.device_id === id);
			return d ? label(d) : id;
		});
		const who = names.length <= 2 ? names.join(' and ') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
		return s ? s.replace('{devices}', who || 'the speaker').replace(/^./, (c) => c.toUpperCase()) : '';
	});
	const others = $derived(speakers.filter((d) => !ids.includes(d.device_id)));
	let dragVol = $state(null);
	// Playlists only when the household has some (or this step already plays one).
	const MODES = $derived([
		['fav', 'Play a favourite'],
		...(lists.length || kind === 'list' ? [['list', 'Play a playlist']] : []),
		['vol', 'Volume'],
		['pause', 'Pause'],
		['group', 'Group']
	]);
</script>

<div class="step">
	{#if said}<p class="said">{said}</p>{/if}

	<span class="seg" role="group" aria-label="What to do">
		{#each MODES as [k, name] (k)}
			<button class:on={kind === k} aria-pressed={kind === k} onclick={() => mode(k)}>{name}</button>
		{/each}
	</span>

	{#if ids.length === 1}
		<label class="hc-field"
			><span class="hc-label">Speaker</span>
			<select value={ids[0]} onchange={(e) => pickSpeaker(e.currentTarget.value)}>
				{#each speakers as d (d.device_id)}<option value={d.device_id}>{label(d)}</option>{/each}
			</select></label
		>
	{:else}
		<p class="hc-quiet">On {names.join(', ')}.</p>
	{/if}

	{#if kind === 'fav'}
		<div class="part">
			<span class="sub">Favourite</span>
			{#if favs.length}
				<div class="favs" role="radiogroup" aria-label="Favourite">
					{#each favs as f (f.title)}
						<button class="fav" class:on={opts.favorite === f.title} role="radio" aria-checked={opts.favorite === f.title} onclick={() => change({ favorite: f.title })}>
							<Art src={f.image} fill radius={8} label={f.title} />
							<span class="name">{f.title}</span>
						</button>
					{/each}
				</div>
			{:else}
				<p class="hc-quiet">The speakers have no favourites yet.</p>
			{/if}
		</div>
	{/if}

	{#if kind === 'list'}
		<div class="part">
			<span class="sub">Playlist</span>
			{#if lists.length}
				<div class="favs" role="radiogroup" aria-label="Playlist">
					{#each lists as f (f.title)}
						<button class="fav" class:on={opts.playlist === f.title} role="radio" aria-checked={opts.playlist === f.title} onclick={() => change({ playlist: f.title })}>
							<Art src={f.image} fill radius={8} label={f.title} />
							<span class="name">{f.title}</span>
						</button>
					{/each}
				</div>
			{:else}
				<p class="hc-quiet">The speakers have no playlists now.</p>
			{/if}
		</div>
	{/if}

	{#if kind === 'fav' || kind === 'vol'}
		<div class="part">
			{#if kind === 'fav'}
				<label class="check"
					><input type="checkbox" checked={opts.withVolume} onchange={(e) => change({ withVolume: e.currentTarget.checked })} /><span>And set the volume to</span></label
				>
			{:else}
				<span class="sub">Volume</span>
			{/if}
			<div class="vol">
				<input
					type="range"
					min="0"
					max="100"
					step="1"
					value={dragVol ?? opts.volume}
					disabled={kind === 'fav' && !opts.withVolume}
					aria-label="Volume"
					oninput={(e) => (dragVol = Number(e.currentTarget.value))}
					onchange={(e) => {
						change({ volume: Number(e.currentTarget.value), withVolume: true });
						dragVol = null;
					}}
				/>
				<span class="hc-num n">{dragVol ?? opts.volume}</span>
			</div>
		</div>
	{/if}

	{#if kind === 'group'}
		<div class="part">
			<span class="sub">Play the speaker's music on these too</span>
			{#each others as d (d.device_id)}
				<label class="check row"><input type="checkbox" checked={opts.members.includes(d.device_id)} onchange={() => toggleMember(d.device_id)} /><span>{label(d)}</span></label>
			{/each}
			<span class="hc-quiet">Speakers left unticked leave the group. Ticking none plays it on its own.</span>
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
		gap: 8px;
	}
	.favs {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 8px;
	}
	.fav {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 8px;
		border-radius: 12px;
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
		min-width: 0;
	}
	.fav.on {
		border: 2px solid var(--accent);
		padding: 7px;
		background: color-mix(in srgb, var(--lit-bg) 35%, var(--surface));
	}
	.name {
		font-size: 13px;
		font-weight: 600;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 14px;
	}
	.check.row {
		min-height: 44px;
		font-size: 15px;
	}
	.check input {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.vol {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.vol input {
		flex: 1;
		accent-color: var(--accent);
	}
	.n {
		font-size: 13px;
		width: 28px;
		text-align: right;
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
