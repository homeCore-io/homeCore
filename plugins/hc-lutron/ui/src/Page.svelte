<svelte:options customElement={{ tag: 'hc-lutron-page', props: { host: { type: 'Object' } } }} />

<!-- Lutron RA2's plugin page: whether the main repeater is connected; the
     scenes on its phantom buttons, lit as the repeater lights them, to run;
     the keypads and remotes, drawn; and a count of everything else. -->
<script>
	import {
		adopt,
		ago,
		buttonsOf,
		ledButtonsIn,
		ledOf,
		listWords,
		pressWord,
		reportsState,
		sceneGroups,
		sceneOn,
		schemaOf,
		settingsHref,
		presses,
		unnamed
	} from './lib/lutron.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const of = (t) => devices.filter((d) => d.device_type === t);
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));
	const canRun = $derived(!!hc?.me.can('devices:command'));

	// The session with the repeater, from the plugin.
	let link = $state(null);
	async function status() {
		const r = await hc.query('status');
		link = r.ok ? r.data : { connected: false, error: r.error };
	}
	$effect(() => {
		if (!hc) return;
		void status();
		const t = setInterval(status, 15000);
		return () => clearInterval(t);
	});

	const scenes = $derived(of('scene'));
	const groups = $derived(sceneGroups(devices));
	const byName = (x, y) => label(x).localeCompare(label(y)) || (x.room ?? '').localeCompare(y.room ?? '');
	const keypads = $derived(of('keypad').sort(byName));
	const picos = $derived(of('pico_remote').sort(byName));
	const receivers = $derived(of('vcrx').sort(byName));

	// Keypad buttons nobody named, keypad by keypad, for the note at the
	// top — the fewest first, since those are the ones someone forgot. A
	// visor receiver's buttons are never engraved, so it is not nagged.
	const nameless = $derived(
		keypads
			.map((d) => ({ d, all: buttonsOf(d).length, missing: buttonsOf(d).filter(unnamed) }))
			.filter((x) => x.missing.length)
			.sort((x, y) => x.missing.length - y.missing.length)
	);
	const namelessWords = $derived(
		listWords(
			nameless.map((x) =>
				x.missing.length === x.all
					? `every button on ${label(x.d)}`
					: `${label(x.d)}'s button${x.missing.length === 1 ? '' : 's'} ${listWords(x.missing.map((b) => String(b.number)))}`
			)
		)
	);

	// Which keypad buttons have lights, from their schemas.
	let lights = $state({});
	$effect(() => {
		for (const k of keypads) void schemaOf(hc, k.device_id).then((s) => (lights = { ...lights, [k.device_id]: ledButtonsIn(s) }));
	});
	const dots = (k) =>
		buttonsOf(k)
			.filter((b) => b.number < 16)
			.map((b) => ({ n: b.number, s: ledOf(k, b.number) ?? ((lights[k.device_id] ?? []).includes(b.number) ? 0 : undefined) }));

	// Each keypad's last press — held or pressed, and when — from history.
	let lastOf = $state({});
	const lastStamp = $derived(keypads.map((k) => `${k.attributes.last_button}:${k.attributes[`button_${k.attributes.last_button}`]}`).join('|'));
	$effect(() => {
		void lastStamp;
		for (const k of keypads) {
			const b = buttonsOf(k).find((x) => x.number === k.attributes.last_button);
			if (b) void presses(hc, k.device_id, [b], 10).then((p) => (lastOf = { ...lastOf, [k.device_id]: p[0] }));
		}
	});
	const lastLine = (k) => {
		const a = k.attributes;
		const p = lastOf[k.device_id];
		if (p) return `Last: ${p.name}, ${p.how}, ${ago(p.at)}`;
		if (!a.last_button_name) return 'No presses seen yet';
		return `Last: ${a.last_button_name}, ${pressWord(a[`button_${a.last_button}`])}`;
	};

	// Everything else, counted, with what is on now.
	const named = (list) => listWords(list.slice(0, 2).map(label)) + (list.length > 2 ? ` and ${list.length - 2} more` : '');
	const kinds = $derived(
		[
			{ t: 'switch', name: 'Switches', now: (l) => `${l.filter((d) => d.attributes.on).length} on now` },
			{ t: 'light', name: 'Dimmers', now: (l) => `${l.filter((d) => d.attributes.on).length} on now` },
			{
				t: 'fan',
				name: 'Fans',
				now: (l) => {
					const on = l.filter((d) => d.attributes.on);
					return on.length ? `${named(on)} running` : 'All off';
				}
			},
			{ t: 'cover', name: 'Shades', now: (l) => `${l.filter((d) => (d.attributes.position ?? 0) > 0).length} open` },
			{
				t: 'occupancy_sensor',
				name: 'Occupancy groups',
				now: (l) => {
					const on = l.filter((d) => d.attributes.occupied === true);
					const room = (d) => d.room ?? label(d).replace(/ Occupancy$/, '');
					return on.length ? `${listWords(on.slice(0, 2).map(room))}${on.length > 2 ? ` and ${on.length - 2} more` : ''} occupied` : 'Nobody anywhere';
				}
			},
			{ t: 'timeclock_event', name: 'Timeclock events', now: (l) => `${l.filter((d) => d.attributes.enabled !== false).length} enabled` }
		]
			.map((k) => ({ ...k, list: of(k.t) }))
			.filter((k) => k.list.length)
	);

	let running = $state('');
	let error = $state('');
	async function run(s) {
		running = s.device_id;
		error = '';
		const r = await hc.command(s.device_id, { action: 'activate' });
		if (!r.ok) error = r.error;
		setTimeout(() => (running = ''), 1200);
	}
	const sceneState = (s) => (running === s.device_id ? 'Running…' : sceneOn(s) ? 'On now' : 'Run');
	const goSettings = () => hc.ui.navigate(settingsHref(hc));
	const goDevices = () => hc.ui.navigate(`/settings/plugins/${encodeURIComponent(hc.pluginId)}?tab=devices`);
</script>

<div class="page">
	<header class="top">
		<div class="status">
			{#if link}
				<span class="pill" class:bad={!link.connected}><span class="hc-dot {link.connected ? 'good' : 'bad'}"></span>{link.connected ? 'Connected' : 'Not connected'}</span>
			{/if}
			<span class="hc-quiet"
				>Main repeater{link?.host ? ` ${link.host}` : ''} · {devices.length} devices · {scenes.length} scene{scenes.length === 1 ? '' : 's'}{link &&
				!link.connected &&
				link.error
					? ` · ${link.error}`
					: ''}</span
			>
		</div>
		{#if admin}
			<span class="hc-row">
				<button class="hc-pill big" onclick={goSettings}>Read the repeater's layout</button>
				<button class="hc-pill big" onclick={goSettings}>Settings</button>
			</span>
		{/if}
	</header>

	{#if nameless.length}
		<div class="note">
			<div class="words">
				<strong>Some keypad buttons have no name yet</strong>
				<span class="hc-quiet">{namelessWords} show as “Button N” in automations. Name them from their engravings.</span>
			</div>
			{#if admin}<button class="link" onclick={goSettings}>Name them ›</button>{/if}
		</div>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	{#if scenes.length}
		<section>
			<div class="head">
				<h2>Scenes</h2>
				<span class="hc-quiet">Phantom buttons on the main repeater · the light shows which is on</span>
			</div>
			{#each groups as g (g.name)}
				<div class="group">
					{#if groups.length > 1}<div class="section-label">{g.name}</div>{/if}
					<div class="scenes">
						{#each g.scenes as s (s.device_id)}
							<button
								class="scene"
								class:on={sceneOn(s)}
								aria-pressed={reportsState(s) ? sceneOn(s) : undefined}
								disabled={!canRun || s.available === false}
								onclick={() => run(s)}
							>
								<span class="led" class:on={sceneOn(s)} class:none={!reportsState(s)}></span>
								<span class="words">
									<b>{label(s)}</b>
									<span class="hc-quiet">{sceneState(s)}</span>
								</span>
							</button>
						{/each}
					</div>
				</div>
			{/each}
		</section>
	{/if}

	{#if keypads.length || picos.length || receivers.length}
		<section>
			<h2>Keypads and remotes</h2>
			{#if keypads.length}
				<div class="grid wide">
					{#each keypads as k (k.device_id)}
						<button class="tile" onclick={() => hc.ui.openDevice(k.device_id)}>
							<span class="mini" aria-hidden="true">
								{#each dots(k) as d (d.n)}
									<span class="mrow"><span class="dot" class:on={d.s > 0} class:none={d.s === undefined}></span><span class="bar"></span></span>
								{/each}
							</span>
							<span class="words">
								<b>{label(k)}</b>
								{#if k.room}<span class="hc-quiet">{k.room}</span>{/if}
								<span class="hc-quiet">{lastLine(k)}</span>
							</span>
						</button>
					{/each}
				</div>
			{/if}
			{#if picos.length || receivers.length}
				<div class="grid wide">
					{#each picos as p (p.device_id)}
						{@const n = buttonsOf(p).length}
						<button class="tile small" onclick={() => hc.ui.openDevice(p.device_id)}>
							<span class="pico" class:tall={n > 2} aria-hidden="true"></span>
							<span class="words">
								<b>{label(p)}</b>
								<span class="hc-quiet">{n ? `${n}-button Pico` : 'Pico'}{p.room ? ` · ${p.room}` : ''}</span>
							</span>
						</button>
					{/each}
					{#each receivers as v (v.device_id)}
						<button class="tile small" onclick={() => hc.ui.openDevice(v.device_id)}>
							<span class="visor" aria-hidden="true"></span>
							<span class="words">
								<b>{label(v)}</b>
								<span class="hc-quiet">Visor receiver{v.room ? ` · ${v.room}` : ''}</span>
							</span>
						</button>
					{/each}
				</div>
			{/if}
		</section>
	{/if}

	{#if kinds.length}
		<section>
			<h2>Everything else</h2>
			<div class="grid">
				{#each kinds as k (k.t)}
					<button class="tile count" onclick={goDevices}>
						<span class="n hc-num">{k.list.length}</span>
						<b>{k.name}</b>
						<span class="hc-quiet">{k.now(k.list)}</span>
					</button>
				{/each}
			</div>
		</section>
	{/if}

	{#if !devices.length}
		<p class="hc-quiet">No devices yet. Read the repeater's layout in Settings, check the rows, and save.</p>
	{/if}
</div>

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
	.group {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.scenes {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(170px, 1fr));
		gap: 10px;
	}
	.scene {
		display: flex;
		align-items: center;
		gap: 12px;
		min-height: 64px;
		padding: 10px 14px;
		border-radius: var(--radius, 14px);
		background: var(--surface);
		border: 1px solid var(--card-line);
		text-align: left;
	}
	.scene.on {
		background: color-mix(in srgb, var(--track-on) 9%, var(--surface));
		border-color: color-mix(in srgb, var(--track-on) 55%, var(--surface));
	}
	.scene:disabled {
		cursor: default;
	}
	.led {
		flex: none;
		width: 10px;
		height: 10px;
		border-radius: 50%;
		background: var(--track-off);
	}
	.led.on {
		background: var(--track-on);
		box-shadow: 0 0 0 4px color-mix(in srgb, var(--track-on) 28%, transparent);
	}
	.led.none {
		background: none;
		border: 1.5px dashed var(--track-off);
	}
	.words {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 2px;
		min-width: 0;
	}
	.words b {
		font-weight: 600;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
		gap: 12px;
	}
	.grid.wide {
		grid-template-columns: repeat(auto-fill, minmax(250px, 1fr));
	}
	.tile {
		display: flex;
		gap: 14px;
		align-items: center;
		padding: 14px;
		border-radius: var(--radius, 14px);
		background: var(--surface);
		border: 1px solid var(--card-line);
		text-align: left;
		min-width: 0;
	}
	.tile.small {
		gap: 12px;
		padding: 12px 14px;
	}
	.tile.count {
		flex-direction: column;
		align-items: flex-start;
		gap: 4px;
		padding: 14px 16px;
	}
	.n {
		font-size: 26px;
		font-weight: 500;
		line-height: 1.2;
	}
	.mini {
		flex: none;
		width: 46px;
		padding: 8px 6px;
		border-radius: 8px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 60%, var(--surface));
		border: 1px solid var(--line);
		display: flex;
		flex-direction: column;
		gap: 5px;
		align-items: center;
	}
	.mrow {
		display: flex;
		align-items: center;
		gap: 4px;
	}
	.dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--track-off);
	}
	.dot.on {
		background: var(--track-on);
	}
	.dot.none {
		background: none;
	}
	.bar {
		width: 22px;
		height: 6px;
		border-radius: 2px;
		background: var(--line);
	}
	.pico {
		flex: none;
		width: 22px;
		height: 28px;
		border-radius: 11px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 60%, var(--surface));
		border: 1px solid var(--line);
	}
	.pico.tall {
		height: 44px;
	}
	.visor {
		flex: none;
		width: 22px;
		height: 40px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 60%, var(--surface));
		border: 1px solid var(--line);
	}
	.tile .hc-quiet,
	.scene .hc-quiet {
		font-size: 13px;
	}
	@container (max-width: 520px) {
		.scenes {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.grid {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.grid.wide {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
