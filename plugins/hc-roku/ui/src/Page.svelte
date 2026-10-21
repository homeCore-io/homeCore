<svelte:options customElement={{ tag: 'hc-roku-page', props: { host: { type: 'Object' } } }} />

<!-- Roku's plugin page: how many Rokus and whether they answer, with "Look
     for Rokus now" and Settings; a card per Roku — what it shows, its most
     used channels to open, where and what it is — with its remote and
     "Refresh channels"; then what to do about a Roku that refuses control,
     and about one that moved or is gone. -->
<script>
	import Tile from './lib/Tile.svelte';
	import {
		adopt,
		appsOf,
		ago,
		every,
		inputsOf,
		listWords,
		mostUsed,
		nowOf,
		openApp,
		rokusOf,
		settingsHref,
		stateLine
	} from './lib/roku.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const rokus = $derived(rokusOf(devices, label));
	const canRun = $derived(!!hc?.me.can('devices:command'));
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));

	// What the plugin knows of each from memory: model, address, Roku OS…
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
		}, 15000);
		return () => clearInterval(t);
	});
	const about = (d) => status?.rokus?.find((x) => x.hc_id === d.device_id) ?? null;
	const quiet = $derived(rokus.filter((d) => d.available === false));
	const refusing = $derived(
		rokus.filter((d) => about(d)?.phone_control === 'disabled' && d.available !== false)
	);

	let error = $state('');
	let busy = $state('');
	async function run(p) {
		error = '';
		const r = await p;
		if (r && !r.ok) error = r.error;
		else if (r?.data?.status === 'error') error = r.data.error;
		return r;
	}
	let looking = $state(false);
	async function look() {
		looking = true;
		await run(hc.action('discover_devices'));
		setTimeout(() => {
			looking = false;
			void readStatus();
		}, 6000);
	}
	async function refresh(d) {
		busy = d.device_id;
		const r = await run(hc.action('refresh_catalog', { hc_id: d.device_id }));
		const said = r?.ok ? r.data?.devices?.[d.device_id] : null;
		if (said?.error) error = `${label(d)}: ${said.error}`;
		busy = '';
		void readStatus();
	}
	async function forget(d) {
		const ok = await hc.ui.confirm({
			title: `Forget ${label(d)}?`,
			body: 'For a Roku that is gone for good. One that is only switched off or unplugged comes back by itself when it answers.',
			lost: ['Its room', 'Its place in automations and on dashboards'],
			go: 'Forget',
			danger: true
		});
		if (ok) await run(hc.action('forget_device', { hc_id: d.device_id }));
	}

	const pill = $derived(
		!rokus.length
			? 'No Rokus yet'
			: quiet.length
				? `${rokus.length - quiet.length} of ${rokus.length} answering`
				: `${rokus.length} Roku${rokus.length === 1 ? '' : 's'} · all answering`
	);
	const looks = $derived(
		status
			? status.discovery
				? `Looks for new Rokus on the network ${every(status.discovery_interval_secs)}${status.last_sweep ? ` · last ${ago(status.last_sweep, tick)}` : ''}`
				: 'Network discovery is off — the Rokus in Settings are asked directly'
			: ''
	);
	const nowStyle = (d) => {
		const n = nowOf(d);
		return n.kind === 'app' && n.playing
			? 'playing'
			: n.kind === 'off' || n.kind === 'offline'
				? 'quiet'
				: '';
	};
	const words = (net) =>
		net === 'ethernet' ? 'Ethernet' : net === 'wifi' ? 'Wi-Fi' : (net ?? '—');
	const channelsWords = (d, s) => {
		const apps = appsOf(d).length;
		const inputs = inputsOf(d).length;
		if (!apps && !inputs) return 'read when it next answers · inputs too';
		return inputs ? `${apps} · inputs ${inputs}` : `${apps}`;
	};
</script>

<div class="page">
	<header class="top">
		<span class="pill" class:warn={quiet.length && rokus.length} class:bad={!rokus.length}
			><span class="dot"></span>{pill}</span
		>
		<span class="hc-quiet looks">{looks}</span>
		{#if canRun}<button class="hc-pill big" disabled={looking} onclick={look}
				>{looking ? 'Looking…' : 'Look for Rokus now'}</button
			>{/if}
		{#if admin}<button class="hc-pill big" onclick={() => hc.ui.navigate(settingsHref(hc))}
				>Settings</button
			>{/if}
	</header>
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	<div class="grid">
		{#each rokus as d (d.device_id)}
			{@const s = about(d)}
			{@const n = nowOf(d)}
			{@const strip = mostUsed(d, 5)}
			<div class="card" class:off={d.available === false}>
				<button class="who" onclick={() => hc.ui.openDevice(d.device_id)}>
					<span class="pic">
						{#if n.kind === 'app'}
							<Tile {hc} app={n.app} radius={12} />
						{:else}
							<span class="plain" class:input={n.kind === 'input'}
								>{n.kind === 'input'
									? n.name
									: n.kind === 'off'
										? 'OFF'
										: n.kind === 'home'
											? 'Home'
											: ''}</span
							>
						{/if}
					</span>
					<span class="words">
						<b>{label(d)}</b>
						<span class="now {nowStyle(d)}"
							>{n.kind === 'off'
								? s?.can_wake || s?.power_mode === 'Ready'
									? 'Off (standby) · can be woken'
									: 'Off (standby)'
								: stateLine(d, tick)}</span
						>
					</span>
				</button>
				{#if strip.length}
					<div class="strip">
						{#each strip as app (app.id)}
							<button
								class="chan"
								aria-label={`Open ${app.name} on ${label(d)}`}
								disabled={!canRun || d.available === false}
								onclick={() => run(openApp(hc, d, app))}
								><Tile {hc} {app} radius={8} lit={n.app?.id === app.id} /></button
							>
						{/each}
					</div>
				{/if}
				<dl class="facts">
					<dt>Room</dt>
					<dd>{d.room ?? 'Not in a room'}</dd>
					<dt>Model</dt>
					<dd>{s?.model ? `${s.model}${s.is_tv ? ' · TV' : ''}` : '—'}</dd>
					<dt>Address</dt>
					<dd class="mono">{s?.address ?? '—'}{s?.network === 'ethernet' ? ' · wired' : ''}</dd>
					<dt>Network</dt>
					<dd>{words(s?.network)}</dd>
					<dt>Roku OS</dt>
					<dd>{s?.roku_os ?? '—'}</dd>
					<dt>Phone control</dt>
					<dd
						class:good={s?.phone_control === 'allowed'}
						class:bad={s?.phone_control === 'disabled'}
					>
						{s?.phone_control === 'allowed'
							? 'Allowed'
							: s?.phone_control === 'disabled'
								? 'Disabled'
								: '—'}
					</dd>
					<dt>Channels</dt>
					<dd>{channelsWords(d, s)}</dd>
					{#if s?.last_error}
						<dt>Last error</dt>
						<dd class="bad small">{s.last_error}</dd>
					{/if}
				</dl>
				<div class="hc-row">
					<button class="hc-pill" onclick={() => hc.ui.openDevice(d.device_id)}
						>Open the remote</button
					>
					{#if canRun}<button
							class="hc-pill"
							disabled={busy === d.device_id || d.available === false}
							onclick={() => refresh(d)}
							>{busy === d.device_id ? 'Reading…' : 'Refresh channels'}</button
						>{/if}
					{#if admin && d.available === false && s && !s.configured}<button
							class="hc-pill danger"
							onclick={() => forget(d)}>Forget it</button
						>{/if}
				</div>
			</div>
		{/each}
	</div>
	{#if !rokus.length}
		<p class="hc-quiet">
			No Rokus yet. Rokus on this network are found by themselves; one on another network is added
			by its address in Settings.
		</p>
	{/if}

	<section class="box warnbox" class:hot={refusing.length}>
		<strong
			>{refusing.length
				? `${listWords(refusing.map(label))} ${refusing.length === 1 ? 'says' : 'say'} “Control by mobile apps: disabled”`
				: 'If a Roku says “Control by mobile apps: disabled”'}</strong
		>
		<span class="hc-quiet"
			>It shows here and as an alert, with the fix: on the Roku, Settings › System › Advanced system
			settings › Control by mobile apps › Network access: Default (or Permissive).</span
		>
	</section>

	<section class="box">
		<strong>A Roku that's moved or gone</strong>
		<span class="hc-quiet"
			>A Roku that changes address is found again by its serial number and keeps its name, room and
			automations. One that stops answering shows offline here — never removed — with “Forget it”
			for one that's gone for good.</span
		>
		{#if quiet.length}
			<span class="quiet-now"
				>Not answering now: {listWords(quiet.map(label))}{quiet.some((d) => about(d)?.configured)
					? ' — one listed in Settings is removed there'
					: ''}.</span
			>
		{/if}
	</section>
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 22px;
		container-type: inline-size;
	}
	.top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
	}
	.pill {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		padding: 6px 12px;
		border-radius: 999px;
		font-size: 14px;
		font-weight: 600;
		background: light-dark(#e6f0e1, #1f3a26);
		color: light-dark(#2f5a27, #8fd19e);
	}
	.pill .dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: currentColor;
	}
	.pill.warn {
		background: light-dark(#fbf0d9, #3a3020);
		color: light-dark(#7a5a12, #e6c27a);
	}
	.pill.bad {
		background: var(--alert-bg);
		color: var(--alert);
	}
	.looks {
		flex: 1 1 320px;
		font-size: 14px;
	}
	.big {
		min-height: 44px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(330px, 1fr));
		gap: 14px;
		align-items: start;
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		min-width: 0;
	}
	.card.off .who,
	.card.off .strip {
		opacity: 0.6;
	}
	.who {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 0;
		border: none;
		background: none;
		text-align: left;
		min-width: 0;
		color: inherit;
	}
	.pic {
		flex: none;
		width: 48px;
	}
	.plain {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 48px;
		height: 48px;
		border-radius: 12px;
		background: #5d6470;
		color: #ffffff;
		font-size: 11px;
		font-weight: 600;
		text-align: center;
	}
	.plain.input {
		background: #3d4a5c;
	}
	.pic :global(.tile) {
		width: 48px;
		height: 48px;
		aspect-ratio: auto;
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.words b {
		font-weight: 600;
	}
	.now {
		font-size: 14px;
	}
	.now.playing {
		color: light-dark(#2f5a27, #8fd19e);
		font-weight: 600;
	}
	.now.quiet {
		color: var(--ink-muted);
	}
	.strip {
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 6px;
	}
	.chan {
		padding: 0;
		border: none;
		background: none;
		min-width: 0;
	}
	.chan:disabled {
		cursor: default;
	}
	.facts {
		display: grid;
		grid-template-columns: 110px minmax(0, 1fr);
		row-gap: 6px;
		margin: 0;
		font-size: 13px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.mono {
		font-family: var(--font-num, monospace);
		font-size: 12px;
	}
	.good {
		color: light-dark(#2f5a27, #8fd19e);
		font-weight: 600;
	}
	.bad {
		color: var(--alert);
		font-weight: 600;
	}
	.small {
		font-size: 12px;
		font-weight: 400;
	}
	.card .hc-pill {
		min-height: 40px;
	}
	.box {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 16px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.box strong {
		font-weight: 600;
	}
	.box .hc-quiet {
		font-size: 14px;
	}
	.warnbox {
		background: light-dark(#fbf3e4, #2f2818);
		border-color: light-dark(#efdcbb, #4a3d22);
	}
	.warnbox.hot {
		border-color: color-mix(in srgb, var(--alert) 45%, var(--surface));
	}
	.quiet-now {
		font-size: 14px;
		font-weight: 600;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	@container (max-width: 520px) {
		.grid {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
