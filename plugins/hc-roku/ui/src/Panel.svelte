<svelte:options
	customElement={{
		tag: 'hc-roku-panel',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A Roku's panel: power; what it shows, with its icon and the video's
     progress; "Open a channel" — the most used, with each channel's own icon,
     the open one lit; then the remote, every channel A–Z, and (on a TV) its
     inputs. The app draws the Roku's name and room above. -->
<script>
	import Icon from './lib/Icon.svelte';
	import Tile from './lib/Tile.svelte';
	import {
		adopt,
		appsOf,
		az,
		canFindRemote,
		clock,
		inputsOf,
		isOn,
		isTv,
		mostUsed,
		nowOf,
		openApp,
		positionNow,
		search,
		switchInput
	} from './lib/roku.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const now = $derived(dev ? nowOf(dev) : { kind: 'offline', name: '' });
	const tv = $derived(isTv(dev));
	const apps = $derived(dev ? appsOf(dev) : []);
	const inputs = $derived(dev ? inputsOf(dev) : []);
	const quick = $derived(dev ? mostUsed(dev, 6) : []);
	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);
	const awake = $derived(now.kind !== 'off' && now.kind !== 'offline');

	// Its model, from the plugin's status (not in device state).
	let model = $state('');
	$effect(() => {
		if (!hc || !deviceId) return;
		let gone = false;
		hc.query('status').then((r) => {
			const mine = r.ok ? r.data?.rokus?.find((x) => x.hc_id === deviceId) : null;
			if (!gone && mine?.model) model = `${mine.model}${mine.is_tv ? ' · TV' : ''}`;
		});
		return () => (gone = true);
	});

	// The video's progress moves on between reports.
	let tick = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (tick = Date.now()), 1000);
		return () => clearInterval(t);
	});
	const where = $derived(dev && now.kind === 'app' ? positionNow(dev, tick) : null);

	let error = $state('');
	// A command homeCore took can still fail at the Roku; the plugin then says
	// why on the device (`last_command_error`). Shown when it appears after
	// something sent from here.
	let sentAt = $state(0);
	let lastSaid = $state(undefined);
	$effect(() => {
		const said = a.last_command_error ?? null;
		if (lastSaid !== undefined && said && said !== lastSaid && Date.now() - sentAt < 20000)
			error = said;
		lastSaid = said;
	});
	async function run(p) {
		error = '';
		sentAt = Date.now();
		lastSaid = a.last_command_error ?? null;
		const r = await p;
		if (r && !r.ok) error = r.error;
	}
	const send = (body) => run(hc.command(deviceId, body));

	let tab = $state('remote');
	const tabs = $derived([
		['remote', 'Remote'],
		['channels', 'All channels'],
		...(tv ? [['inputs', 'Inputs']] : [])
	]);
	let q = $state('');
	const listed = $derived(search(az(apps), q));
	let text = $state('');
	const typeIt = () => {
		const t = text.trim();
		if (!t) return;
		void send({ action: 'text', text: t });
		text = '';
	};
	const nowWords = $derived(
		now.kind === 'off'
			? 'Off'
			: now.kind === 'offline'
				? 'Not answering'
				: now.kind === 'home'
					? 'Home screen'
					: now.name
	);
	const nowNote = $derived(
		now.kind === 'input'
			? now.live
				? `Live TV · channel ${now.live}`
				: 'An input — what plays on it is up to the device plugged in.'
			: now.kind === 'off'
				? 'Standby. Opening a channel turns it on first.'
				: now.kind === 'offline'
					? 'Unplugged or fully off. It comes back when it answers.'
					: now.kind === 'home'
						? 'Nothing open.'
						: now.paused
							? 'Paused'
							: now.playing
								? ''
								: 'Open, nothing playing.'
	);
</script>

{#if dev}
	<div class="panel" class:off={dev.available === false}>
		<div class="head">
			<span class="hc-quiet model">{model}</span>
			<button
				class="power"
				class:on={isOn(dev)}
				aria-label={isOn(dev) ? 'Turn off' : 'Turn on'}
				aria-pressed={isOn(dev)}
				disabled={!hc?.me.can('devices:command')}
				onclick={() => send({ on: !isOn(dev) })}><Icon name="power" size={20} /></button
			>
		</div>

		<div class="now">
			<span class="now-tile">
				{#if now.kind === 'app'}
					<Tile {hc} app={now.app} radius={8} />
				{:else}
					<span
						class="plain"
						style:background={now.kind === 'input' ? '#3d4a5c' : 'var(--track-off)'}
						style:color={now.kind === 'input' ? '#ffffff' : 'var(--ink-muted)'}
						>{now.kind === 'input' ? now.name : now.kind === 'home' ? 'Home' : ''}</span
					>
				{/if}
			</span>
			<span class="now-words">
				<b>{nowWords}</b>
				{#if where && where.dur}
					<span class="progress">
						<span class="bar"
							><span class="fill" style:width={`${(where.pos / where.dur) * 100}%`}></span></span
						>
						<span class="hc-num times">{clock(where.pos)} / {clock(where.dur)}</span>
					</span>
				{:else if where}
					<span class="hc-num times">{clock(where.pos)}</span>
				{/if}
				{#if nowNote}<span class="hc-quiet note">{nowNote}</span>{/if}
			</span>
		</div>

		{#if apps.length}
			<div class="open">
				<div class="open-head">
					<span class="label">Open a channel</span>
					<button class="all" onclick={() => (tab = 'channels')}>All {apps.length} ›</button>
				</div>
				<div class="quick">
					{#each quick as app (app.id)}
						<button
							class="chan"
							aria-label={`Open ${app.name}`}
							disabled={!canRun}
							onclick={() => run(openApp(hc, dev, app))}
						>
							<Tile {hc} {app} lit={a.app_id === app.id && awake} />
							<span class="name">{app.name}</span>
						</button>
					{/each}
				</div>
				<span class="hc-quiet caption"
					>Most used first, with each channel's own icon from the Roku. The lit one is open now.</span
				>
			</div>
		{:else}
			<p class="hc-quiet">Its channels are read the next time it answers.</p>
		{/if}

		<div class="seg" role="tablist" aria-label="Show">
			{#each tabs as [id, name] (id)}
				<button
					role="tab"
					class:on={tab === id}
					aria-selected={tab === id}
					onclick={() => (tab = id)}>{name}</button
				>
			{/each}
		</div>

		{#if tab === 'remote'}
			<div class="remote">
				{#if !awake}<p class="hc-quiet">
						{now.kind === 'off'
							? 'It is off — turn it on to use the remote.'
							: 'It isn’t answering.'}
					</p>{/if}
				<div class="row">
					<button class="key" disabled={!canRun || !awake} onclick={() => send({ action: 'back' })}
						>Back</button
					>
					<button class="key" disabled={!canRun || !awake} onclick={() => send({ action: 'home' })}
						>Home</button
					>
				</div>
				<div class="pad">
					<span></span>
					<button
						class="dir"
						aria-label="Up"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'up' })}><Icon name="up" /></button
					>
					<span></span>
					<button
						class="dir"
						aria-label="Left"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'left' })}><Icon name="left" /></button
					>
					<button class="ok" disabled={!canRun || !awake} onclick={() => send({ action: 'select' })}
						>OK</button
					>
					<button
						class="dir"
						aria-label="Right"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'right' })}><Icon name="right" /></button
					>
					<span></span>
					<button
						class="dir"
						aria-label="Down"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'down' })}><Icon name="down" /></button
					>
					<span></span>
				</div>
				<div class="row">
					<button
						class="key small"
						aria-label="Instant replay"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'instant_replay' })}><Icon name="replay" /></button
					>
					<button
						class="key small"
						aria-label="Options"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'info' })}>✱</button
					>
					<button
						class="key small"
						aria-label="Rewind"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'rewind' })}><Icon name="rewind" /></button
					>
					<button
						class="key play"
						aria-label="Play or pause"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'play_pause' })}><Icon name="play-pause" /></button
					>
					<button
						class="key small"
						aria-label="Fast forward"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'fast_forward' })}><Icon name="forward" /></button
					>
				</div>
				<div class="row">
					<span class="hc-quiet vol">Volume</span>
					<button
						class="key small"
						aria-label="Volume down"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'volume_down' })}>−</button
					>
					<button
						class="key"
						class:muted={a.muted === true}
						aria-pressed={a.muted === true}
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'set_mute', muted: !a.muted })}
						>{a.muted ? 'Muted' : 'Mute'}</button
					>
					<button
						class="key small"
						aria-label="Volume up"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'volume_up' })}>+</button
					>
				</div>
				<form
					class="type"
					onsubmit={(e) => {
						e.preventDefault();
						typeIt();
					}}
				>
					<label class="hc-field"
						><span class="label">Type on the screen</span>
						<input
							type="text"
							placeholder="e.g. a search"
							maxlength="200"
							bind:value={text}
							disabled={!canRun || !awake}
						/></label
					>
					<button class="key" type="submit" disabled={!canRun || !awake || !text.trim()}
						>Send</button
					>
				</form>
				{#if canFindRemote(dev)}
					<button
						class="key find"
						disabled={!canRun || !awake}
						onclick={() => send({ action: 'find_remote' })}>Find the remote (it beeps)</button
					>
				{/if}
			</div>
		{:else if tab === 'channels'}
			<div class="channels">
				<label class="hc-field"
					><span class="label">Find a channel</span>
					<input type="search" placeholder="e.g. PBS" bind:value={q} /></label
				>
				<div class="grid">
					{#each listed as app (app.id)}
						<button
							class="chan"
							aria-label={`Open ${app.name}`}
							disabled={!canRun}
							onclick={() => run(openApp(hc, dev, app))}
						>
							<Tile {hc} {app} lit={a.app_id === app.id && awake} />
							<span class="name strong">{app.name}</span>
						</button>
					{/each}
				</div>
				{#if !listed.length}<p class="hc-quiet">No channel called “{q}”.</p>{/if}
				<span class="hc-quiet caption"
					>All the channels installed on this Roku, A–Z. Tap to open — a TV that's off is turned on
					first. Kept when the TV is off, so automations can still pick from them.</span
				>
			</div>
		{:else}
			<div class="inputs">
				<div class="pills">
					{#each inputs as i (i.id)}
						<button
							class="pill"
							class:on={a.app_id === i.id && awake}
							aria-pressed={a.app_id === i.id && awake}
							disabled={!canRun}
							onclick={() => run(switchInput(hc, dev, i))}>{i.name}</button
						>
					{/each}
				</div>
				{#if !inputs.length}<p class="hc-quiet">
						Its inputs are read the next time it answers.
					</p>{/if}
				<span class="hc-quiet caption">The TV's inputs by the names you gave them on the TV.</span>
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
		max-width: 600px;
	}
	.panel.off {
		opacity: 0.6;
	}
	.head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 44px;
	}
	.model {
		font-size: 14px;
	}
	.power {
		flex: none;
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
	.power.on {
		color: var(--accent);
		border-color: color-mix(in srgb, var(--accent) 50%, var(--surface));
	}
	.now {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 12px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.now-tile {
		flex: none;
		width: 72px;
	}
	.plain {
		display: flex;
		align-items: center;
		justify-content: center;
		aspect-ratio: 4 / 3;
		border-radius: 8px;
		font-size: 12px;
		font-weight: 700;
		text-align: center;
		padding: 4px;
		box-sizing: border-box;
	}
	.now-words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}
	.now-words b {
		font-weight: 600;
	}
	.note {
		font-size: 13px;
	}
	.progress {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.bar {
		flex: 1;
		height: 4px;
		border-radius: 2px;
		background: color-mix(in srgb, var(--track-off) 45%, var(--surface));
		overflow: hidden;
	}
	.fill {
		display: block;
		height: 100%;
		background: var(--accent);
	}
	.times {
		font-size: 12px;
		color: var(--ink-muted);
	}
	.open {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.open-head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}
	.label {
		font-size: 13px;
		font-weight: 600;
		color: var(--ink);
	}
	.all {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
		font-size: 14px;
	}
	.quick {
		display: grid;
		grid-template-columns: repeat(6, minmax(0, 1fr));
		gap: 8px;
	}
	.chan {
		display: flex;
		flex-direction: column;
		gap: 4px;
		align-items: stretch;
		padding: 0;
		border: none;
		background: transparent;
		min-width: 0;
	}
	.chan:disabled {
		cursor: default;
	}
	.chan .name {
		font-size: 11px;
		color: var(--ink-muted);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		text-align: center;
	}
	.chan .name.strong {
		color: var(--ink);
	}
	.caption {
		font-size: 12px;
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
	.remote {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
	}
	.remote > p {
		margin: 0;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		align-items: center;
		gap: 8px;
	}
	.key {
		min-height: 44px;
		min-width: 76px;
		padding: 0 12px;
		border-radius: 12px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
	.key.small {
		min-width: 52px;
	}
	.key.play {
		min-width: 64px;
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
	}
	.key.muted {
		color: var(--alert);
		border-color: var(--alert-line, var(--line));
	}
	.key:disabled,
	.dir:disabled,
	.ok:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.vol {
		font-size: 13px;
	}
	.pad {
		display: grid;
		grid-template-columns: repeat(3, 64px);
		grid-template-rows: repeat(3, 64px);
		gap: 6px;
	}
	.dir {
		border-radius: 14px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.ok {
		border-radius: 50%;
		border: none;
		background: #4a2a72;
		color: #ffffff;
		font-weight: 700;
	}
	.type {
		display: flex;
		gap: 8px;
		width: 100%;
	}
	.type .hc-field {
		flex: 1;
	}
	.type .key {
		align-self: flex-end;
		min-width: 0;
	}
	.hc-field {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.hc-field input {
		min-height: 44px;
		border-radius: 10px;
		padding: 0 12px;
		box-sizing: border-box;
	}
	.find {
		min-height: 40px;
	}
	.channels,
	.inputs {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(5, minmax(0, 1fr));
		gap: 10px;
		max-height: 470px;
		overflow-y: auto;
		padding: 4px 4px 4px 2px;
	}
	.pills {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.pill {
		min-height: 44px;
		padding: 0 16px;
		border-radius: 11px;
		font-size: 14px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.pill.on {
		border: 2px solid var(--accent);
		padding: 0 15px;
		background: color-mix(in srgb, var(--lit-bg) 40%, var(--surface));
		color: var(--lit-ink, var(--accent));
		font-weight: 600;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 14px;
	}
	p {
		margin: 0;
	}
	@media (max-width: 480px) {
		.quick {
			gap: 6px;
		}
		.grid {
			grid-template-columns: repeat(4, minmax(0, 1fr));
		}
	}
</style>
