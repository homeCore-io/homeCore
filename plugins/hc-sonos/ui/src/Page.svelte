<svelte:options customElement={{ tag: 'hc-sonos-page', props: { host: { type: 'Object' } } }} />

<!-- Sonos's plugin page: how many speakers and when they were found; the
     speakers as the groups they play in — a group is one card, with its
     music, one volume for the group and each speaker's own; the favourites,
     to play on the speakers you tick; and the Play together sheet. -->
<script>
	import Art from './lib/Art.svelte';
	import Icon from './lib/Icon.svelte';
	import Volume from './lib/Volume.svelte';
	import Together from './lib/Together.svelte';
	import {
		adopt,
		ago,
		devicesHref,
		favouritesOf,
		playlistsOf,
		groupsOf,
		groupVolumeOf,
		hasMusic,
		isPlaying,
		listWords,
		makeGroup,
		nowLines,
		art,
		playPause,
		settingsHref,
		speakersOf,
		volumeOf
	} from './lib/sonos.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const speakers = $derived(speakersOf(devices, label));
	const groups = $derived(groupsOf(devices, label));
	const favs = $derived(favouritesOf(speakers));
	const lists = $derived(playlistsOf(speakers));
	const canRun = $derived(!!hc?.me.can('devices:command'));
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));

	// When discovery last looked, from the plugin.
	let status = $state(null);
	let tick = $state(Date.now());
	async function readStatus() {
		const r = await hc.query('status');
		if (r.ok) status = r.data;
	}
	$effect(() => {
		if (!hc) return;
		void readStatus();
		const t = setInterval(() => {
			tick = Date.now();
			void readStatus();
		}, 30000);
		return () => clearInterval(t);
	});
	const found = $derived(
		status?.last_sweep ? `${status.ssdp === false ? 'Found at the listed addresses' : 'Found on the network'} ${ago(status.last_sweep, tick)}` : ''
	);

	const roomless = $derived(speakers.filter((d) => !d.room));
	// Not answering: kept, shown offline, back when they answer.
	const quiet = $derived(speakers.filter((d) => d.available === false));

	async function forget(d) {
		const ok = await hc.ui.confirm({
			title: `Forget ${label(d)}?`,
			body: 'For a speaker that is gone for good. One that is only switched off comes back by itself when it answers.',
			lost: ['Its room', 'Its place in automations and on dashboards'],
			go: 'Forget',
			danger: true
		});
		if (!ok) return;
		error = '';
		const r = await hc.action('forget_speaker', { device_id: d.device_id });
		if (!r.ok) error = r.error;
		else if (r.data?.status === 'error') error = r.data.error;
	}

	// The household's alarms.
	let alarms = $state([]);
	let alarmBusy = $state('');
	async function readAlarms() {
		const r = await hc.query('list_alarms');
		if (r.ok && Array.isArray(r.data?.alarms)) alarms = r.data.alarms;
	}
	$effect(() => {
		if (!hc) return;
		void readAlarms();
		const t = setInterval(readAlarms, 120000);
		return () => clearInterval(t);
	});
	async function switchAlarm(al, enabled) {
		alarmBusy = al.id;
		error = '';
		const r = await hc.action('set_alarm', { id: al.id, enabled });
		if (!r.ok) error = r.error;
		else if (r.data?.status === 'error') error = r.data.error;
		await readAlarms();
		alarmBusy = '';
	}

	let error = $state('');
	async function run(p) {
		error = '';
		const r = await p;
		if (r && !r.ok) error = r.error;
	}
	const setVolume = (d, v) => run(hc.command(d.device_id, { volume: v }));
	const setGroupVolume = (g, v) => run(hc.command(g.leader.device_id, { action: 'set_group_volume', volume: v }));

	// Something to play — a favourite or a playlist — and the speakers it plays on.
	let pickedFav = $state(null);
	let pickedKind = $state('fav');
	const fav = $derived(
		pickedKind === 'list'
			? (lists.find((f) => f.title === pickedFav) ?? lists[0])
			: (favs.find((f) => f.title === pickedFav) ?? favs[0])
	);
	const pick = (kind, title) => {
		pickedKind = kind;
		pickedFav = title;
	};
	let targets = $state(null);
	const chosen = $derived(targets ?? speakers.filter((d) => isPlaying(d)).map((d) => d.device_id).slice(0, 1));
	const toggleTarget = (id) => (targets = chosen.includes(id) ? chosen.filter((x) => x !== id) : [...chosen, id]);
	let playing = $state(false);
	async function playFav() {
		if (!fav || !chosen.length) return;
		playing = true;
		error = '';
		// A grouped speaker's favourite plays on its whole group; say it once.
		const leaders = new Set();
		const sends = [];
		for (const id of chosen) {
			const d = devices.find((x) => x.device_id === id);
			const lead = d?.attributes?.group_coordinator ?? id;
			if (leaders.has(lead)) continue;
			leaders.add(lead);
			sends.push(
				hc.command(
					id,
					pickedKind === 'list' ? { action: 'play_playlist', playlist: fav.title } : { action: 'play_favorite', favorite: fav.title }
				)
			);
		}
		const out = await Promise.all(sends);
		const bad = out.find((r) => !r.ok);
		if (bad) error = bad.error;
		setTimeout(() => (playing = false), 800);
	}

	// The Play together sheet.
	let sheet = $state(null);
	const openSheet = (g) => {
		const lead = g?.leader ?? groups.find((x) => isPlaying(x.leader))?.leader ?? speakers[0];
		sheet = { leader: lead?.device_id, ticked: (g?.members ?? speakers).map((d) => d.device_id) };
	};
	const ungroup = (g) => run(makeGroup(hc, g.leader.device_id, []));

	async function rescan() {
		error = '';
		const r = await hc.action('rediscover_speakers');
		if (!r.ok) error = r.error;
		setTimeout(readStatus, 4000);
	}
</script>

<div class="page">
	<header class="top">
		<div class="status">
			<span class="pill" class:bad={!speakers.length} class:warn={speakers.length && quiet.length}
				><span class="hc-dot {!speakers.length ? 'bad' : quiet.length ? 'warn' : 'good'}"></span>{!speakers.length
					? 'No speakers yet'
					: quiet.length
						? `${speakers.length - quiet.length} of ${speakers.length} answering`
						: `${speakers.length} speaker${speakers.length === 1 ? '' : 's'}`}</span
			>
			<span class="hc-quiet">{[found, favs.length ? `${favs.length} favourite${favs.length === 1 ? '' : 's'}` : '', lists.length ? `${lists.length} playlist${lists.length === 1 ? '' : 's'}` : ''].filter(Boolean).join(' · ')}</span>
		</div>
		<span class="hc-row">
			{#if canRun}<button class="hc-pill big" onclick={rescan}>Look for speakers</button>{/if}
			{#if admin}<button class="hc-pill big" onclick={() => hc.ui.navigate(settingsHref(hc))}>Settings</button>{/if}
		</span>
	</header>

	{#if roomless.length}
		<div class="note">
			<div class="words">
				<strong>{roomless.length === speakers.length ? "Your speakers aren't in a room yet" : `${listWords(roomless.map(label))} ${roomless.length === 1 ? "isn't" : "aren't"} in a room yet`}</strong>
				<span class="hc-quiet">{listWords(roomless.map(label))} show{roomless.length === 1 ? 's' : ''} under no room on Home.</span>
			</div>
			{#if admin}<button class="link" onclick={() => hc.ui.navigate(devicesHref(hc))}>Place them ›</button>{/if}
		</div>
	{/if}
	{#if quiet.length}
		<div class="note quiet">
			<div class="words">
				<strong>{listWords(quiet.map(label))} {quiet.length === 1 ? "isn't" : "aren't"} answering</strong>
				<span class="hc-quiet">{quiet.length === 1 ? 'It stays' : 'They stay'} here, shown offline, with {quiet.length === 1 ? 'its room and automations' : 'their rooms and automations'}, and come{quiet.length === 1 ? 's' : ''} back on {quiet.length === 1 ? 'its' : 'their'} own. Gone for good? Forget {quiet.length === 1 ? 'it' : 'them'}.</span>
			</div>
			{#if admin}
				<span class="hc-row">
					{#each quiet as d (d.device_id)}<button class="hc-pill danger" onclick={() => forget(d)}>Forget {label(d)}</button>{/each}
				</span>
			{/if}
		</div>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	<section>
		<div class="head">
			<h2>Speakers</h2>
			{#if canRun && speakers.length > 1}<button class="hc-pill" onclick={() => openSheet(null)}>Group speakers…</button>{/if}
		</div>
		<div class="grid">
			{#each groups as g (g.id)}
				{@const d = g.leader}
				{@const lines = nowLines(d, { empty: favs.length ? 'Pick a favourite below' : 'Nothing queued' })}
				<div class="card" class:wide={g.grouped} class:lit={isPlaying(d) || d.attributes.state === "paused"} class:off={d.available === false}>
					<div class="top-row">
						<button class="open" aria-label={`Open ${g.name}`} onclick={() => hc.ui.openDevice(d.device_id)}>
							<Art src={art(d)} size={56} label={lines.title} />
							<span class="words">
								<b>{g.grouped ? g.name : label(d)}</b>
								<span class="line1">{lines.title}</span>
								<span class="line2 hc-quiet">{lines.sub}</span>
							</span>
						</button>
						<button
							class="play"
							aria-label={`${isPlaying(d) ? 'Pause' : 'Play'} ${g.name}`}
							disabled={!canRun || !hasMusic(d) || d.available === false}
							onclick={() => run(playPause(hc, d))}><Icon name={isPlaying(d) ? 'pause' : 'play'} size={16} /></button
						>
					</div>
					{#if g.grouped}
						<Volume label="Group" strong value={groupVolumeOf(d) ?? volumeOf(d)} disabled={!canRun} onset={(v) => setGroupVolume(g, v)} />
						{#each g.members as m (m.device_id)}
							<Volume label={label(m)} value={volumeOf(m)} disabled={!canRun || m.available === false} onset={(v) => setVolume(m, v)} />
						{/each}
						{#if canRun}
							<div class="hc-row">
								<button class="hc-pill" onclick={() => openSheet(g)}>Change the group</button>
								<button class="hc-pill" onclick={() => ungroup(g)}>Ungroup</button>
							</div>
						{/if}
					{:else}
						<Volume icon name={`${label(d)} volume`} value={volumeOf(d)} disabled={!canRun || d.available === false} onset={(v) => setVolume(d, v)} />
					{/if}
				</div>
			{/each}
		</div>
		{#if !speakers.length}
			<p class="hc-quiet">No speakers found yet. Sonos speakers are found on the network; list their addresses in Settings if they are on another one.</p>
		{/if}
	</section>

	{#if favs.length || lists.length}
		<section>
			{#if favs.length}
			<div class="head">
				<h2>Favourites</h2>
				<span class="hc-quiet">Your Sonos favourites · tap to choose where it plays</span>
			</div>
			{/if}
			<div class="favs">
				{#each favs as f (f.title)}
					{@const on = pickedKind === 'fav' && fav?.title === f.title}
					<button class="fav" class:on aria-pressed={on} onclick={() => pick('fav', f.title)}>
						<Art src={f.image} fill radius={10} label={f.title} />
						<b>{f.title}</b>
					</button>
				{/each}
			</div>
			{#if lists.length}
				<div class="head lists">
					<h2>Playlists</h2>
					<span class="hc-quiet">Your Sonos playlists</span>
				</div>
				<div class="favs">
					{#each lists as f (f.title)}
						{@const on = pickedKind === 'list' && fav?.title === f.title}
						<button class="fav" class:on aria-pressed={on} onclick={() => pick('list', f.title)}>
							<Art src={f.image} fill radius={10} label={f.title} />
							<b>{f.title}</b>
						</button>
					{/each}
				</div>
			{/if}
			{#if fav && canRun}
				<div class="bar">
					<b class="bar-title">Play {fav.title} on</b>
					{#each speakers as d (d.device_id)}
						<button class="chip" class:on={chosen.includes(d.device_id)} aria-pressed={chosen.includes(d.device_id)} onclick={() => toggleTarget(d.device_id)}
							>{label(d)}</button
						>
					{/each}
					<button class="hc-pill go playfav" disabled={!chosen.length || playing} onclick={playFav}>{playing ? 'Playing…' : 'Play'}</button>
				</div>
			{/if}
		</section>
	{/if}

	{#if alarms.length}
		<section>
			<div class="head">
				<h2>Alarms</h2>
				<span class="hc-quiet">Set in the Sonos app · switch one off for a day off</span>
			</div>
			<div class="alarms hc-card">
				{#each alarms as al (al.id)}
					<label class="alarm" class:offal={!al.enabled}>
						<span class="when hc-num">{al.time}</span>
						<span class="what">
							<b>{al.room ?? 'Another speaker'} · {al.repeats}</b>
							<span class="hc-quiet">{al.plays}{al.volume != null ? ` at volume ${al.volume}` : ''}</span>
						</span>
						<input type="checkbox" checked={al.enabled} disabled={!canRun || alarmBusy === al.id} aria-label={`Alarm at ${al.time}`} onchange={(e) => switchAlarm(al, e.currentTarget.checked)} />
					</label>
				{/each}
			</div>
		</section>
	{/if}
</div>

{#if sheet}
	<Together {hc} devices={speakers} leader={sheet.leader} ticked={sheet.ticked} onclose={() => (sheet = null)} />
{/if}

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 28px;
		container-type: inline-size;
	}
	section {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	h2 {
		margin: 0;
		font-size: 20px;
		font-weight: 600;
	}
	.top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
	}
	.status {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.status .hc-quiet {
		font-size: 14px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 10px;
		border-radius: 11px;
		font-size: 14px;
		font-weight: 600;
		background: light-dark(#e6f0e1, #1f3a26);
		color: light-dark(#2f5a27, #8fd19e);
	}
	.pill.bad {
		background: var(--alert-bg);
		color: var(--alert);
	}
	.pill.warn {
		background: light-dark(#fbf0d9, #3a3020);
		color: light-dark(#7a5a12, #e6c27a);
	}
	.note.quiet {
		background: color-mix(in srgb, var(--alert-bg, #fdecea) 55%, var(--surface));
		border-color: color-mix(in srgb, var(--alert) 25%, var(--surface));
	}
	.alarms {
		display: flex;
		flex-direction: column;
		padding: 4px 16px;
	}
	.alarm {
		display: flex;
		align-items: center;
		gap: 14px;
		min-height: 56px;
		border-bottom: 1px solid var(--line);
	}
	.alarm:last-child {
		border-bottom: none;
	}
	.alarm .when {
		font-size: 20px;
		font-weight: 600;
		min-width: 58px;
	}
	.alarm .what {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.alarm .what b {
		font-weight: 600;
		font-size: 15px;
	}
	.alarm .what .hc-quiet {
		font-size: 13px;
	}
	.alarm.offal .when,
	.alarm.offal b {
		color: var(--ink-muted);
	}
	.alarm input {
		flex: none;
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.pill .hc-dot {
		width: 8px;
		height: 8px;
	}
	.big {
		min-height: 44px;
	}
	.note {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 14px 18px;
		border-radius: var(--radius, 14px);
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
	}
	.note .words {
		flex: 1 1 320px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.note strong {
		font-weight: 600;
	}
	.note .hc-quiet {
		font-size: 14px;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.head {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		justify-content: space-between;
		gap: 4px 12px;
	}
	.head .hc-quiet {
		font-size: 14px;
	}
	.head .hc-pill {
		min-height: 40px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
		gap: 12px;
		align-items: start;
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 14px;
		border-radius: var(--radius, 14px);
		background: var(--surface);
		border: 1px solid var(--card-line);
		min-width: 0;
	}
	.card.wide {
		grid-column: span 2;
	}
	.card.lit {
		background: color-mix(in srgb, var(--lit-bg) 40%, var(--surface));
		border-color: color-mix(in srgb, var(--track-on) 55%, var(--surface));
	}
	.card.off {
		opacity: 0.6;
	}
	.card .hc-row .hc-pill {
		min-height: 40px;
	}
	.top-row {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.open {
		flex: 1;
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
		padding: 0;
		border: none;
		background: none;
		text-align: left;
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
		flex: 1;
	}
	.words b {
		font-weight: 600;
	}
	.line1,
	.line2 {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.line1 {
		font-size: 14px;
	}
	.words .line2 {
		font-size: 13px;
	}
	.play {
		flex: none;
		width: 44px;
		height: 44px;
		border-radius: 50%;
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.play:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.favs {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
		gap: 12px;
	}
	.fav {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 12px;
		border-radius: var(--radius, 14px);
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
		min-width: 0;
	}
	.fav :global(.art) {
		height: 80px !important;
		aspect-ratio: auto;
	}
	.fav.on {
		border: 2px solid var(--accent);
		padding: 11px;
	}
	.fav b {
		font-weight: 600;
	}
	.bar {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
		padding: 14px 16px;
		border-radius: var(--radius, 14px);
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.bar-title {
		font-weight: 600;
	}
	.chip {
		min-height: 40px;
		padding: 0 14px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--surface);
	}
	.chip.on {
		background: color-mix(in srgb, var(--lit-bg) 40%, var(--surface));
		border-color: color-mix(in srgb, var(--track-on) 60%, var(--surface));
		font-weight: 600;
	}
	.playfav {
		margin-left: auto;
		min-height: 40px;
		padding: 0 16px;
	}
	@container (max-width: 560px) {
		.card.wide {
			grid-column: auto;
		}
	}
	@container (max-width: 520px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
		.favs {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.playfav {
			margin-left: 0;
			width: 100%;
			justify-content: center;
		}
	}
	.head.lists {
		margin-top: 12px;
	}
</style>
