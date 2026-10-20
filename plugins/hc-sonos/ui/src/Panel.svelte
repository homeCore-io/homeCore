<svelte:options
	customElement={{
		tag: 'hc-sonos-panel',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A speaker's panel: what it plays — a track with its progress, or a radio
     station, live — the transport, mute and volume (and the group's), then
     its favourites, its group and its sound. The app draws the speaker's
     name above. -->
<script>
	import Art from './lib/Art.svelte';
	import Icon from './lib/Icon.svelte';
	import Volume from './lib/Volume.svelte';
	import {
		adopt,
		art,
		clock,
		favouritesOf,
		groupIdsOf,
		groupName,
		groupVolumeOf,
		hasMusic,
		inputOf,
		isPlaying,
		isRadio,
		leaderIdOf,
		makeGroup,
		num,
		playbackWord,
		playPause,
		positionNow,
		RESULT_WORDS,
		SLEEP_CHOICES,
		sleepLeft,
		sleepWords,
		speakersOf,
		volumeOf
	} from './lib/sonos.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const speakers = $derived(speakersOf(devices, label));
	const favs = $derived(favouritesOf(speakers));
	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);

	const radio = $derived(isRadio(dev));
	const music = $derived(hasMusic(dev));
	const track = $derived(music && !radio && !inputOf(dev));
	const favNamed = $derived(favs.some((f) => f.title === a.media_title));
	const kind = $derived(music ? playbackWord(dev) : '');
	const input = $derived(inputOf(dev));
	const sub = $derived(
		input
			? 'Input'
			: !music
			? favs.length
				? 'Pick a favourite below'
				: ''
			: radio
				? favNamed
					? 'Radio station — a favourite'
					: 'Radio station'
				: [a.media_artist, a.media_album].filter(Boolean).join(' · ')
	);
	const groupIds = $derived(groupIdsOf(dev ?? { device_id: deviceId }));
	const grouped = $derived(groupIds.length > 1);
	const leaderId = $derived(dev ? leaderIdOf(dev) : deviceId);
	const groupWords = $derived(
		grouped ? groupName([leaderId, ...groupIds.filter((id) => id !== leaderId)].map((id) => devices.find((d) => d.device_id === id)).filter(Boolean).map(label)) : ''
	);

	// The progress bar moves on its own between reports.
	let seen = $state(null);
	let now = $state(Date.now());
	$effect(() => {
		const p = num(a.media_position) ?? 0;
		if (!seen || seen.id !== deviceId || seen.pos !== p) seen = { id: deviceId, pos: p, at: Date.now() };
	});
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});
	const where = $derived(dev ? positionNow(dev, seen, now) : { pos: 0, dur: 0 });

	let error = $state('');
	// A command homeCore took can still fail at the speaker; the plugin then
	// says why on the speaker (`last_command_error`). Shown when it appears
	// after something sent from here.
	let sentAt = $state(0);
	let lastSaid = $state(undefined);
	$effect(() => {
		const said = a.last_command_error ?? null;
		if (lastSaid !== undefined && said && said !== lastSaid && Date.now() - sentAt < 15000) error = said;
		lastSaid = said;
	});
	async function send(body) {
		error = '';
		sentAt = Date.now();
		lastSaid = a.last_command_error ?? null;
		const r = await hc.command(deviceId, body);
		if (!r.ok) error = r.error;
	}

	// The inputs it can be switched to, from its schema's `select_source`.
	let inputs = $state([]);
	$effect(() => {
		if (!hc || !deviceId) return;
		let gone = false;
		hc.fetch(`/devices/${encodeURIComponent(deviceId)}/schema`)
			.then((r) => (r.ok ? r.json() : null))
			.then((schema) => {
				if (gone || !schema) return;
				const act = (schema.actions ?? []).find((x) => x.id === 'select_source');
				const p = act?.params?.find((x) => x.name === 'source');
				inputs = (p?.options ?? []).map((o) => (typeof o === 'string' ? o : o.value)).filter((v) => typeof v === 'string' && v);
			})
			.catch(() => {});
		return () => (gone = true);
	});
	const sleepMins = $derived(dev ? sleepLeft(dev, now) : 0);
	const toggle = async () => {
		error = '';
		const r = await playPause(hc, dev);
		if (!r.ok) error = r.error;
	};
	const nextRepeat = { none: 'all', all: 'one', one: 'none' };
	function seek(e) {
		if (!track || !where.dur || !canRun) return;
		const box = e.currentTarget.getBoundingClientRect();
		const f = Math.min(1, Math.max(0, (e.clientX - box.left) / box.width));
		void send({ action: 'seek', position: Math.round(f * where.dur) });
	}

	let tab = $state('fav');
	const TABS = [
		['fav', 'Favourites'],
		['group', 'Group'],
		['sound', 'Sound']
	];

	// The Group tab: the other speakers, ticked when they play this group's
	// music. A tick changes the group at once and says what happened.
	const others = $derived(speakers.filter((d) => d.device_id !== deviceId));
	let busyId = $state('');
	let answers = $state({});
	const nowOf = (d) => (d.available === false ? 'Not answering' : hasMusic(d) ? `${inputOf(d) || d.attributes.media_title}, ${playbackWord(d)}` : 'Nothing playing');
	async function tick(o) {
		busyId = o.device_id;
		error = '';
		const inGroup = groupIds.includes(o.device_id);
		let r;
		if (o.device_id === leaderId) {
			// Unticking the leader: this speaker leaves its group.
			r = await hc.command(deviceId, { action: 'unjoin' });
			answers = { ...answers, [o.device_id]: r.ok ? 'left' : 'failed' };
		} else {
			const members = groupIds.filter((id) => id !== leaderId && id !== o.device_id);
			if (!inGroup) members.push(o.device_id);
			const g = await makeGroup(hc, leaderId, members);
			if (!g.ok) error = g.error;
			const mine = g.results.find((x) => x.device_id === o.device_id);
			answers = { ...answers, [o.device_id]: mine?.result ?? (g.ok ? (inGroup ? 'left' : 'joined') : 'failed') };
		}
		busyId = '';
	}
</script>

{#if dev}
	<div class="panel" class:off={dev.available === false}>
		<div class="now">
			<Art src={art(dev)} size={120} radius={12} label={a.media_title ?? ''} />
			<div class="words">
				{#if kind}<span class="kind">{kind}</span>{/if}
				<span class="title">{input || (music ? a.media_title : 'Nothing playing')}</span>
				{#if sub}<span class="hc-quiet sub">{sub}</span>{/if}
				{#if grouped}<span class="hc-quiet sub">Playing together: {groupWords}</span>{/if}
				{#if sleepMins}<span class="hc-quiet sub">{sleepWords(sleepMins)}</span>{/if}
			</div>
		</div>

		{#if track && where.dur}
			<div class="progress">
				<button class="bar" aria-label="Seek" disabled={!canRun} onclick={seek}><span class="fill" style:width={`${(where.pos / where.dur) * 100}%`}></span></button>
				<div class="times hc-num"><span>{clock(where.pos)}</span><span>{clock(where.dur)}</span></div>
			</div>
		{:else if radio}
			<div class="live"><span class="dot"></span>Live — no track to skip</div>
		{/if}

		<div class="transport">
			<button class="round dim" class:active={a.shuffle === true} aria-label="Shuffle" aria-pressed={a.shuffle === true} disabled={!canRun || !track} onclick={() => send({ action: 'set_shuffle', shuffle: !a.shuffle })}
				><Icon name="shuffle" /></button
			>
			<button class="round" aria-label="Previous" disabled={!canRun || !track} onclick={() => send({ action: 'previous' })}><Icon name="prev" /></button>
			<button class="main" aria-label={isPlaying(dev) ? 'Pause' : 'Play'} disabled={!canRun || !music} onclick={toggle}
				><Icon name={isPlaying(dev) ? 'pause' : 'play'} size={22} /></button
			>
			<button class="round" aria-label="Next" disabled={!canRun || !track} onclick={() => send({ action: 'next' })}><Icon name="next" /></button>
			<button
				class="round dim"
				class:active={a.repeat === 'all' || a.repeat === 'one'}
				aria-label={`Repeat${a.repeat === 'one' ? ' one' : a.repeat === 'all' ? ' all' : ''}`}
				disabled={!canRun || !track}
				onclick={() => send({ action: 'set_repeat', repeat: nextRepeat[a.repeat] ?? 'all' })}
				><Icon name="repeat" />{#if a.repeat === 'one'}<span class="one">1</span>{/if}</button
			>
		</div>

		<div class="volume">
			<button class="mute" class:on={a.muted === true} aria-label={a.muted ? 'Unmute' : 'Mute'} aria-pressed={a.muted === true} disabled={!canRun} onclick={() => send({ muted: !a.muted })}
				><Icon name={a.muted ? 'muted' : 'volume'} /></button
			>
			<div class="sliders">
				<Volume
					label={grouped && groupVolumeOf(dev) !== undefined ? label(dev) : ''}
					value={volumeOf(dev)}
					disabled={!canRun}
					onset={(v) => send({ volume: v })}
				/>
				{#if grouped && groupVolumeOf(dev) !== undefined}
					<Volume label="Group" strong value={groupVolumeOf(dev)} disabled={!canRun} onset={(v) => send({ action: 'set_group_volume', volume: v })} />
				{/if}
			</div>
		</div>

		<div class="seg" role="tablist" aria-label="More">
			{#each TABS as [id, name] (id)}
				<button role="tab" class:on={tab === id} aria-selected={tab === id} onclick={() => (tab = id)}>{name}</button>
			{/each}
		</div>

		{#if tab === 'fav'}
			{#if favs.length}
				<div class="favs">
					{#each favs as f (f.title)}
						<button class="fav" class:on={music && a.media_title === f.title} disabled={!canRun} onclick={() => send({ action: 'play_favorite', favorite: f.title })}>
							<Art src={f.image} fill radius={8} label={f.title} />
							<b>{f.title}</b>
						</button>
					{/each}
				</div>
			{:else}
				<p class="hc-quiet">No favourites yet — add some in the Sonos app.</p>
			{/if}
		{:else if tab === 'group'}
			<div class="group">
				<span class="hc-quiet lead">Play {label(dev)}'s music in these rooms too</span>
				{#each others as o (o.device_id)}
					{@const inGroup = groupIds.includes(o.device_id)}
					{@const said = answers[o.device_id]}
					<label class="row">
						<input type="checkbox" checked={inGroup} disabled={!canRun || busyId === o.device_id} onchange={() => tick(o)} />
						<span class="name">{label(o)}</span>
						{#if o.device_id === leaderId && inGroup}<span class="from">playing from</span>{/if}
						<span class="hc-quiet what" class:bad={said === 'no_answer' || said === 'failed'}
							>{busyId === o.device_id ? 'asking…' : said ? (RESULT_WORDS[said] ?? said) : nowOf(o)}</span
						>
					</label>
				{/each}
			</div>
		{:else}
			<div class="sound">
				<Volume label="Bass" min={-10} max={10} value={num(a.bass) ?? 0} disabled={!canRun} onset={(v) => send({ action: 'set_bass', bass: v })} />
				<Volume label="Treble" min={-10} max={10} value={num(a.treble) ?? 0} disabled={!canRun} onset={(v) => send({ action: 'set_treble', treble: v })} />
				<label class="loud">
					<span>Loudness — fuller sound at low volume</span>
					<input type="checkbox" checked={a.loudness === true} disabled={!canRun} onchange={(e) => send({ action: 'set_loudness', loudness: e.currentTarget.checked })} />
				</label>
				{#if a.night_mode !== undefined}
					<label class="loud">
						<span>Night mode — quieter loud parts, clearer quiet ones</span>
						<input type="checkbox" checked={a.night_mode === true} disabled={!canRun} onchange={(e) => send({ action: 'set_night_mode', night_mode: e.currentTarget.checked })} />
					</label>
				{/if}
				{#if a.speech_enhancement !== undefined}
					<label class="loud">
						<span>Speech enhancement — voices stand out</span>
						<input
							type="checkbox"
							checked={a.speech_enhancement === true}
							disabled={!canRun}
							onchange={(e) => send({ action: 'set_speech_enhancement', speech_enhancement: e.currentTarget.checked })}
						/>
					</label>
				{/if}
				<label class="loud">
					<span>Crossfade between tracks{grouped ? ' — for the group' : ''}</span>
					<input type="checkbox" checked={a.crossfade === true} disabled={!canRun} onchange={(e) => send({ action: 'set_crossfade', crossfade: e.currentTarget.checked })} />
				</label>
				<label class="loud">
					<span>Sleep timer{sleepMins ? ` — ${sleepWords(sleepMins).toLowerCase()}` : ''}</span>
					<select
						class="pick"
						disabled={!canRun}
						value={sleepMins ? 'on' : '0'}
						onchange={(e) => {
							const m = Number(e.currentTarget.value);
							if (Number.isFinite(m)) void send({ action: 'set_sleep_timer', minutes: m });
						}}
					>
						{#if sleepMins}<option value="on" disabled>{sleepMins} min left</option>{/if}
						{#each SLEEP_CHOICES as m (m)}<option value={String(m)}>{m ? `${m} min` : 'Off'}</option>{/each}
					</select>
				</label>
				{#if inputs.length}
					<div class="loud">
						<span>Play from</span>
						<span class="hc-row">
							{#each inputs as src (src)}
								<button class="hc-pill" class:go={a.source === src} aria-pressed={a.source === src} disabled={!canRun || a.source === src} onclick={() => send({ action: 'select_source', source: src })}
									>{src}</button
								>
							{/each}
						</span>
					</div>
				{/if}
			</div>
		{/if}

		{#if error}<p class="err" role="alert">{error}</p>{/if}
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 16px;
		max-width: 560px;
	}
	.panel.off {
		opacity: 0.6;
	}
	.now {
		display: flex;
		gap: 16px;
		align-items: center;
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.kind {
		font-size: 12px;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--ink-muted);
	}
	.title {
		font-size: 20px;
		font-weight: 600;
		line-height: 1.25;
	}
	.sub {
		font-size: 14px;
	}
	.progress {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.bar {
		display: block;
		width: 100%;
		height: 6px;
		padding: 0;
		border: none;
		border-radius: 3px;
		background: color-mix(in srgb, var(--track-off) 45%, var(--surface));
		overflow: hidden;
	}
	.bar:disabled {
		cursor: default;
	}
	.fill {
		display: block;
		height: 100%;
		background: var(--accent);
	}
	.times {
		display: flex;
		justify-content: space-between;
		font-size: 12px;
		color: var(--ink-muted);
	}
	.live {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 13px;
		color: var(--ink-muted);
	}
	.live .dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: #c0392b;
	}
	.transport {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 14px;
	}
	.round {
		position: relative;
		width: 44px;
		height: 44px;
		border-radius: 50%;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.round.dim {
		color: var(--ink-muted);
	}
	.round.active {
		color: var(--accent);
		border-color: color-mix(in srgb, var(--accent) 50%, var(--surface));
	}
	.round:disabled {
		color: color-mix(in srgb, var(--ink-muted) 45%, var(--surface));
		cursor: default;
	}
	.one {
		position: absolute;
		right: 7px;
		bottom: 6px;
		font-size: 9px;
		font-weight: 700;
	}
	.main {
		width: 60px;
		height: 60px;
		border-radius: 50%;
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.main:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.volume {
		display: flex;
		align-items: flex-start;
		gap: 10px;
	}
	.sliders {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
		padding-top: 12px;
	}
	.mute {
		flex: none;
		width: 44px;
		height: 44px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink-muted);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.mute.on {
		color: var(--alert);
		border-color: var(--alert-line, var(--line));
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
	}
	.fav b {
		font-size: 13px;
		font-weight: 600;
	}
	.group {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.group .lead {
		font-size: 13px;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 44px;
		padding: 0 4px;
		font-size: 15px;
	}
	.row input {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.row .name {
		flex: 1;
	}
	.what {
		font-size: 13px;
		text-align: right;
	}
	.what.bad {
		color: var(--alert);
		font-weight: 600;
	}
	.from {
		font-size: 12px;
		font-weight: 600;
		color: var(--lit-ink, var(--accent));
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--track-on) 55%, var(--surface));
		padding: 2px 8px;
		border-radius: 9px;
	}
	.sound {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.sound :global(.label) {
		width: 56px;
		font-size: 14px;
		color: var(--ink);
	}
	.loud {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		min-height: 44px;
		font-size: 14px;
	}
	.loud input {
		flex: none;
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.loud .pick {
		flex: none;
		min-width: 120px;
		height: 36px;
		padding: 0 8px;
		border: 1px solid var(--line);
		border-radius: var(--radius-chip, 9px);
		background: var(--surface);
		color: var(--ink);
		font: inherit;
		font-size: 14px;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 14px;
	}
	@media (max-width: 420px) {
		.now {
			gap: 12px;
		}
		.transport {
			gap: 8px;
		}
	}
</style>
