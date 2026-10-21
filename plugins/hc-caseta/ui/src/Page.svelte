<svelte:options customElement={{ tag: 'hc-caseta-page', props: { host: { type: 'Object' } } }} />

<!-- Caséta's plugin page: whether the Smart Bridge is connected and when it
     last said anything; the lights and plugs with what each works as (from
     Settings) and a hint where a "switch" dims; each Pico's buttons and what
     they start; the bridge's scenes to run, with the automations that use
     them; and what the bridge talks about that nobody set up here. -->
<script>
	import { adopt, ago, automationsOn, buttonsOf, loadRules } from './lib/lutron.js';
	import {
		byButton,
		clock,
		heardWords,
		linkWords,
		nowWords,
		settingsHref,
		usedByWords,
		usesOf,
		worksAs
	} from './lib/caseta.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));
	const canRun = $derived(!!hc?.me.can('devices:command'));

	// The session with the bridge, and what it has heard, from the plugin.
	let status = $state(null);
	async function refresh() {
		const r = await hc.query('status');
		status = r.ok ? r.data : { state: 'offline', error: r.error };
	}
	$effect(() => {
		if (!hc) return;
		void refresh();
		const t = setInterval(refresh, 10000);
		return () => clearInterval(t);
	});
	const link = $derived(linkWords(status));

	let rules = $state([]);
	$effect(() => {
		if (hc) void loadRules(hc).then((r) => (rules = r));
	});

	const byName = (x, y) =>
		label(x).localeCompare(label(y)) || (x.room ?? '').localeCompare(y.room ?? '');
	const kindOf = (d) => status?.lights?.find((l) => l.device_id === d.device_id);
	// In the order the settings list them.
	const rank = (list, d) => {
		const i = (list ?? []).findIndex((x) => x.device_id === d.device_id);
		return i < 0 ? 1e6 : i;
	};
	const lights = $derived(
		devices
			.filter((d) => ['light', 'switch', 'fan', 'cover'].includes(d.device_type))
			.sort((x, y) => rank(status?.lights, x) - rank(status?.lights, y) || byName(x, y))
	);
	const onCount = $derived(lights.filter((d) => d.attributes.on === true).length);
	const picos = $derived(
		devices
			.filter((d) => d.device_type === 'pico_remote')
			.sort((x, y) => rank(status?.picos, x) - rank(status?.picos, y) || byName(x, y))
	);
	const scenes = $derived(
		byButton(
			devices.filter((d) => d.device_type === 'scene'),
			status
		)
	);
	const heard = $derived(status?.heard_not_set_up ?? []);
	const picoOf = (d) => status?.picos?.find((p) => p.device_id === d.device_id);

	// What each of a Pico's buttons starts, top to bottom by number.
	const rows = (p) =>
		buttonsOf(p).map((b) => ({ ...b, rules: automationsOn(rules, p.device_id, b.number) }));
	const lastLine = (p) => {
		const last = picoOf(p)?.last;
		if (last) return `last: ${last.name}, ${clock(last.at)}`;
		if (p.attributes.last_button_name) return `last: ${p.attributes.last_button_name}`;
		return 'never pressed';
	};

	let running = $state('');
	let error = $state('');
	async function run(s) {
		running = s.device_id;
		error = '';
		const r = await hc.command(s.device_id, { action: 'activate' });
		if (!r.ok) error = r.error;
		setTimeout(() => (running = ''), 1200);
	}
	let reconnecting = $state(false);
	async function reconnect() {
		reconnecting = true;
		const r = await hc.action('reconnect');
		if (!r.ok) error = r.error;
		setTimeout(() => {
			reconnecting = false;
			void refresh();
		}, 2000);
	}
	const go = (tab) => hc.ui.navigate(settingsHref(hc, tab));
	const openRule = (id) => hc.ui.navigate(`/settings/automations/${encodeURIComponent(id)}`);
</script>

<div class="page">
	<header class="top">
		{#if status}
			<span class="pill" class:bad={!link.good}
				><span class="hc-dot {link.good ? 'good' : 'bad'}"></span>{link.text}</span
			>
		{/if}
		<span class="hc-quiet line"
			>{status?.host || 'No address'}{status?.last_heard
				? ` · heard from it ${ago(status.last_heard)}`
				: ''} · {devices.length} devices{status && !link.good && status.error
				? ` · ${status.error}`
				: ''}</span
		>
		{#if admin}
			<button class="hc-pill big" disabled={reconnecting} onclick={reconnect}
				>{reconnecting ? 'Reconnecting…' : 'Reconnect'}</button
			>
			<button class="hc-pill big" onclick={() => go('settings')}>Settings</button>
		{/if}
	</header>
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	{#if lights.length}
		<section>
			<div class="head">
				<h2>Lights and plugs</h2>
				<span class="hc-quiet">{onCount} of {lights.length} on</span>
			</div>
			<div class="table">
				<table>
					<thead>
						<tr><th>Device</th><th class="room">Room</th><th>Now</th><th>Works as</th></tr>
					</thead>
					<tbody>
						{#each lights as d (d.device_id)}
							{@const k = kindOf(d)}
							<tr>
								<td class="name"
									><button class="link plain" onclick={() => hc.ui.openDevice(d.device_id)}
										>{label(d)}</button
									></td
								>
								<td class="hc-quiet room">{d.room ?? '—'}</td>
								<td>{nowWords(d)}</td>
								<td>
									<span class="hc-quiet">{worksAs(k?.kind)}</span>
									{#if k?.dims_at != null}
										<span class="dims"
											>· dims — {#if admin}<button class="link" onclick={() => go('devices')}
													>set it to Dimmer in Settings?</button
												>{:else}set it to Dimmer in Settings?{/if}</span
										>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
			<span class="hc-quiet foot"
				>“Works as” comes from each device's kind in {#if admin}<button
						class="link"
						onclick={() => go('devices')}>Settings › Devices</button
					>{:else}Settings › Devices{/if}. A device set to on/off that the bridge reports at a part
				level (say 40%) is marked “dims — set it to Dimmer in Settings?”. Change it in the Lutron
				app first, then in Settings.</span
			>
		</section>
	{/if}

	{#if picos.length}
		<section>
			<h2>Pico remotes</h2>
			<div class="picos">
				{#each picos as p (p.device_id)}
					{@const info = picoOf(p)}
					<div class="box">
						<div class="ptop">
							<button class="link plain strong" onclick={() => hc.ui.openDevice(p.device_id)}
								>{label(p)}</button
							>
							<span class="hc-quiet">{p.room ?? 'No room'} · {lastLine(p)}</span>
						</div>
						{#if info?.assumed}
							<span class="hc-quiet note"
								>Set up as a 5-button Pico ({buttonsOf(p)
									.map((b) => b.name)
									.join(', ')}) — the usual one. A 4-button Pico is put right the first time a
								button is pressed; a 2-button one is set in Settings.</span
							>
							{#if !p.room}
								<button class="link strong left" onclick={() => hc.ui.openDevice(p.device_id)}
									>Put it in a room ›</button
								>
							{/if}
						{:else}
							{#each rows(p) as b (b.number)}
								<div class="brow">
									<span class="bname">{b.name}</span>
									<span class="uses">
										{#if b.rules.length}
											{#each b.rules as r (r.id)}<button class="link" onclick={() => openRule(r.id)}
													>{r.name}</button
												>{/each}
										{:else}<span class="hc-quiet">Not used by an automation</span>{/if}
									</span>
								</div>
							{/each}
						{/if}
					</div>
				{/each}
			</div>
			<span class="hc-quiet foot"
				>The bridge runs what a Pico is paired to in the Lutron app; homeCore runs the automations
				listed here as well.</span
			>
		</section>
	{/if}

	{#if scenes.length}
		<section>
			<h2>Bridge scenes</h2>
			<div class="scenes">
				{#each scenes as s (s.device_id)}
					<div class="box scene">
						<span class="words">
							<b>{label(s)}</b>
							<span class="hc-quiet small">{usedByWords(usesOf(rules, s.device_id).length)}</span>
						</span>
						<button
							class="hc-pill run"
							disabled={!canRun || s.available === false || running === s.device_id}
							onclick={() => run(s)}>{running === s.device_id ? 'Running…' : 'Run'}</button
						>
					</div>
				{/each}
			</div>
			<span class="hc-quiet foot"
				>Scenes are made in the Lutron app; homeCore runs them. Renaming one there keeps its
				automations here.</span
			>
		</section>
	{/if}

	<section class="box heard">
		<strong>Heard from the bridge, not set up here</strong>
		{#if heard.length}
			{#each heard as h (h.id)}
				<div class="hrow">
					<span>{heardWords(h)} <span class="hc-quiet">· {h.detail} · {ago(h.at)}</span></span>
					{#if admin}<button class="link strong" onclick={() => go('devices')}>Add it</button>{/if}
				</div>
			{/each}
		{:else}
			<span class="hc-quiet"
				>None. A device the bridge talks about that homeCore doesn't know shows here with “Add it”.</span
			>
		{/if}
	</section>

	{#if !devices.length}
		<p class="hc-quiet">
			No devices yet. Paste the integration report in Settings, check the rows, and save.
		</p>
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 22px;
		container-type: inline-size;
	}
	section {
		display: flex;
		flex-direction: column;
		gap: 12px;
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
		gap: 12px;
	}
	.line {
		flex: 1 1 320px;
		font-size: 14px;
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
	.pill.bad {
		background: var(--alert-bg);
		color: var(--alert);
	}
	.pill .hc-dot {
		width: 8px;
		height: 8px;
	}
	.big {
		min-height: 44px;
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
		gap: 12px;
	}
	.head .hc-quiet,
	.foot {
		font-size: 13px;
	}
	.table {
		overflow-x: auto;
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
	}
	table {
		width: 100%;
		min-width: 560px;
		border-collapse: collapse;
		font-size: 15px;
	}
	th {
		text-align: left;
		font-size: 12px;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--ink-muted);
		font-weight: 600;
		padding: 12px 8px;
	}
	td {
		padding: 10px 8px;
		border-top: 1px solid var(--card-line);
	}
	th:first-child,
	td:first-child {
		padding-left: 16px;
	}
	th:last-child,
	td:last-child {
		padding-right: 16px;
	}
	td.name {
		font-weight: 600;
	}
	.dims {
		font-size: 14px;
		color: var(--warn, var(--alert));
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.link.plain {
		color: inherit;
	}
	.link.strong,
	.strong {
		font-weight: 600;
	}
	.left {
		align-self: flex-start;
		font-size: 14px;
	}
	.box {
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
		padding: 16px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
	}
	.picos {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(330px, 1fr));
		gap: 12px;
	}
	.ptop {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		align-items: baseline;
		gap: 4px 10px;
	}
	.ptop .hc-quiet {
		font-size: 13px;
	}
	.note {
		font-size: 14px;
	}
	.brow {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 36px;
		border-top: 1px solid var(--card-line);
		font-size: 14px;
	}
	.bname {
		width: 70px;
		flex: none;
		font-weight: 600;
	}
	.uses {
		flex: 1;
		display: flex;
		flex-wrap: wrap;
		gap: 2px 12px;
		min-width: 0;
	}
	.scenes {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
		gap: 10px;
	}
	.scene {
		flex-direction: row;
		align-items: center;
		padding: 12px 14px;
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.words b {
		font-weight: 600;
	}
	.small {
		font-size: 12px;
	}
	.run {
		min-height: 40px;
	}
	.heard {
		gap: 8px;
	}
	.heard strong {
		font-weight: 600;
	}
	.heard > .hc-quiet,
	.hrow {
		font-size: 14px;
	}
	.hrow {
		display: flex;
		justify-content: space-between;
		gap: 12px;
	}
	@container (max-width: 520px) {
		table {
			min-width: 0;
		}
		.room {
			display: none;
		}
		.picos,
		.scenes {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
