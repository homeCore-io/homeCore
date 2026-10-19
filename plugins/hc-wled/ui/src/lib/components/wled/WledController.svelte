<!--
	Everything a WLED controller does itself: nightlight, sync with other
	WLEDs, live data, sound — and, for admins, its firmware (what it runs,
	what's out, installing one, from GitHub or a file), its own timers, its
	setup (LED outputs, buttons, network, clock), backups, and restarting it.
-->
<script lang="ts">
	import { host } from '#lib/host.js';
	import type { CoreDevice } from '#lib/devices.js';
	import { commands } from '#lib/commands.svelte.js';
	import { session } from '#lib/me.svelte.js';
	import { ask } from '#lib/confirm.svelte.js';
	import { lifecycle, runAction } from '#lib/pluginAdmin.js';
	import { wledCatalogs } from '#lib/wledCatalog.svelte.js';
	import { catalogStorage, presetsOf, WLED } from '#lib/wled.js';
	import type { Timer } from '#lib/wledSettings.js';
	import WledSound from './WledSound.svelte';
	import WledTimers from './WledTimers.svelte';
	import WledSetup from './WledSetup.svelte';

	let { device, disabled }: { device: CoreDevice; disabled: boolean } = $props();

	const id = $derived(device.device_id);
	const a = $derived(device.attributes);
	const admin = $derived(session.isAdmin);
	const presets = $derived(presetsOf(device));

	// ---- nightlight ----
	type Nl = {
		on?: boolean;
		duration_min?: number;
		mode?: string;
		target_pct?: number;
		remaining_s?: number | null;
	};
	const nl = $derived((a.nightlight ?? {}) as Nl);
	let nlFor = $state<number | undefined>(undefined);
	let nlTo = $state<number | undefined>(undefined);
	let nlMode = $state<string | undefined>(undefined);
	const nightlight = (on: boolean) =>
		commands.act(id, 'nightlight', {
			nightlight_on: on,
			nightlight_duration_min: nlFor ?? nl.duration_min ?? 60,
			nightlight_mode: nlMode ?? nl.mode ?? 'fade',
			nightlight_target_pct: nlTo ?? nl.target_pct ?? 0
		});

	// ---- sync ----
	type Sync = { send?: boolean; receive?: boolean; send_groups?: number; receive_groups?: number };
	const sync = $derived((a.sync ?? {}) as Sync);
	const groupOn = (mask: number | undefined, g: number) => ((mask ?? 0) & (1 << (g - 1))) !== 0;
	const flip = (mask: number | undefined, g: number) => (mask ?? 0) ^ (1 << (g - 1));
	let peers = $state<{ name?: string; ip?: string }[] | undefined>(undefined);
	async function loadPeers() {
		const r = await host().query('nodes', { device_id: id });
		const nodes = r.ok ? r.data.nodes : undefined;
		peers = Array.isArray(nodes) ? nodes : [];
	}

	// ---- firmware ----
	type Offer = { version: string; prerelease: boolean; published: string; notes_url: string };
	type Fw = {
		installed?: string;
		build?: string;
		source?: string;
		state?: string;
		installing?: string;
		failure?: string;
		checked_at?: string;
		offers?: Offer[];
	};
	let fw = $state<Fw | undefined>(undefined);
	let channel = $state('stable');
	let fwMsg = $state('');
	async function loadFirmware() {
		const r = await host().query('firmware_status');
		const data = (r.ok ? r.data : {}) as { devices?: Record<string, Fw>; channel?: string };
		fw = data.devices?.[id] ?? undefined;
		channel = data.channel ?? 'stable';
	}
	$effect(() => {
		void a.firmware_status;
		void loadFirmware();
	});
	const newer = $derived(
		a.firmware_update_available === true ? String(a.firmware_latest ?? '') : ''
	);
	const older = $derived((fw?.offers ?? []).filter((o) => o.version !== fw?.installed));
	let pickVersion = $state('');
	async function install(version: string) {
		if (
			!(await ask({
				title: `Install firmware ${version}? The lights go dark for about a minute; a backup is taken first.`,
				go: 'Install',
				stay: 'Not now'
			}))
		)
			return;
		void commands.act(id, 'install_firmware', { version });
	}
	async function checkNow() {
		fwMsg = 'Looking…';
		const r = await runAction(WLED, 'check_firmware', { device_id: id });
		fwMsg = r.ok ? '' : r.error;
		await loadFirmware();
	}
	async function fromFile(file: File) {
		if (
			!(await ask({
				title: `Install ${file.name}? The lights go dark for about a minute; a backup is taken first.`,
				go: 'Install',
				stay: 'Not now'
			}))
		)
			return;
		fwMsg = `Sending ${file.name}…`;
		// To the plugin, through homeCore: it backs the controller up, sends
		// the file, and waits for it to come back.
		const r = await host().upload('install_firmware_file', file, { device_id: id }, file.name);
		fwMsg = r.ok ? `Done — it runs ${String(r.data.running ?? '?')}.` : r.error;
	}

	// ---- the controller's own settings (admins) ----
	type Sections = Record<string, unknown>;
	let sections = $state<Sections | undefined>(undefined);
	let settingsError = $state('');
	async function loadSettings() {
		const r = await runAction(WLED, 'get_settings', { device_id: id });
		if (!r.ok) return void (settingsError = r.error);
		if (r.data.kind === 'reply') sections = (r.data.reply as { sections?: Sections }).sections;
	}
	$effect(() => {
		if (admin) void loadSettings();
	});
	const timers = $derived((sections?.timers as Timer[] | undefined) ?? []);
	// ---- catalogue (admins) ----
	let catMsg = $state('');
	async function setCatalog(where: 'controller' | 'app') {
		const r = await runAction(WLED, 'update_device', { device_id: id, catalog: where });
		if (!r.ok) return void (catMsg = r.error);
		catMsg = 'Restarting the WLED plugin to take it…';
		const l = await lifecycle(WLED, 'restart');
		catMsg = l.ok ? 'Saved.' : l.error;
	}

	// ---- maintenance ----
	let maintMsg = $state('');
	async function restart() {
		if (
			!(await ask({
				title: 'Restart the controller? Its lights go out for a few seconds.',
				go: 'Restart',
				stay: 'Not now'
			}))
		)
			return;
		void commands.act(id, 'restart');
	}
	async function backupAll() {
		const r = await runAction(WLED, 'backup', { device_id: id });
		if (!r.ok) return void (maintMsg = r.error);
		if (r.data.kind !== 'reply') return;
		const doc = (r.data.reply as { backup?: unknown }).backup;
		const link = document.createElement('a');
		link.href = URL.createObjectURL(
			new Blob([JSON.stringify(doc, null, 2)], { type: 'application/json' })
		);
		link.download = `${id}-backup-${new Date().toISOString().slice(0, 10)}.json`;
		link.click();
		URL.revokeObjectURL(link.href);
		maintMsg = 'Backed up — and kept with the plugin too.';
	}
	async function restoreSettings(file: File) {
		let doc: unknown;
		try {
			doc = JSON.parse(await file.text());
		} catch {
			return void (maintMsg = "That file isn't a backup.");
		}
		if (
			!(await ask({
				title: `Put back the settings in ${file.name}? The controller keeps its own passwords.`,
				go: 'Restore',
				stay: 'Not now'
			}))
		)
			return;
		const r = await runAction(WLED, 'restore', {
			device_id: id,
			backup: doc,
			presets: false,
			settings: true
		});
		maintMsg = r.ok ? 'Settings restored. Some take effect after a restart.' : r.error;
	}
	const fmt = (iso?: string) =>
		iso ? new Date(iso).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' }) : '—';
</script>

<div class="ctl">
	<div class="grid">
		<section class="card">
			<div class="head">
				<h3>Nightlight</h3>
				{#if nl.on}<span class="tag"
						>on{typeof nl.remaining_s === 'number'
							? ` · ${Math.ceil(nl.remaining_s / 60)} min left`
							: ''}</span
					>{/if}
			</div>
			<p class="quiet small">Dims down by itself, on the controller.</p>
			<div class="two">
				<label
					><span class="label">For (min)</span>
					<input
						type="number"
						min="1"
						max="255"
						value={nl.duration_min ?? 60}
						{disabled}
						onchange={(e) => (nlFor = Number(e.currentTarget.value))}
					/></label
				>
				<label
					><span class="label">Down to (%)</span>
					<input
						type="number"
						min="0"
						max="100"
						value={nl.target_pct ?? 0}
						{disabled}
						onchange={(e) => (nlTo = Number(e.currentTarget.value))}
					/></label
				>
			</div>
			<div class="chips" role="radiogroup" aria-label="How">
				{#each [['instant', 'All at once'], ['fade', 'Fade'], ['color', 'Fade the colour'], ['sunrise', 'Sunrise']] as [m, label] (m)}
					<button
						role="radio"
						aria-checked={(nlMode ?? nl.mode ?? 'fade') === m}
						{disabled}
						onclick={() => (nlMode = m)}>{label}</button
					>
				{/each}
			</div>
			<div class="row-buttons">
				<button class="pill" {disabled} onclick={() => nightlight(true)}
					>{nl.on ? 'Start again' : 'Start'}</button
				>
				{#if nl.on}<button class="pill" {disabled} onclick={() => nightlight(false)}>Stop</button
					>{/if}
			</div>
		</section>

		<section class="card">
			<div class="head">
				<h3>Sync with other WLEDs</h3>
				<span class="num">{a.peers_count ?? 0} peers seen</span>
			</div>
			<label class="check row"
				><span>Send changes</span>
				<input
					type="checkbox"
					checked={sync.send === true}
					{disabled}
					onchange={(e) => commands.send(id, { sync_send: e.currentTarget.checked })}
				/></label
			>
			<label class="check row"
				><span>Follow changes from others</span>
				<input
					type="checkbox"
					checked={sync.receive === true}
					{disabled}
					onchange={(e) => commands.send(id, { sync_receive: e.currentTarget.checked })}
				/></label
			>
			{#each [['Sends to groups', 'send_groups', 'sync_send_groups'], ['Follows groups', 'receive_groups', 'sync_receive_groups']] as [label, key, word] (key)}
				{@const mask = sync[key as keyof Sync] as number | undefined}
				<div class="groups">
					<span class="label">{label}</span>
					{#each [1, 2, 3, 4, 5, 6, 7, 8] as g (g)}
						<button
							aria-pressed={groupOn(mask, g)}
							{disabled}
							onclick={() => commands.send(id, { [word as string]: flip(mask, g) })}>{g}</button
						>
					{/each}
				</div>
			{/each}
			{#if peers === undefined}
				<button class="link" onclick={loadPeers}>Which WLEDs it sees</button>
			{:else if peers.length === 0}
				<p class="quiet small">It sees no other WLED on WLED-Sync.</p>
			{:else}
				<ul class="peers">
					{#each peers as p (p.ip)}<li>{p.name} <span class="num">{p.ip}</span></li>{/each}
				</ul>
			{/if}
		</section>

		<section class="card">
			<div class="head">
				<h3>Live data</h3>
				<span class="num"
					>{a.live === true
						? `streaming${a.live_source ? ` from ${a.live_source}` : ''}`
						: 'nothing streaming'}</span
				>
			</div>
			<p class="quiet small">
				E1.31, Art-Net, DDP and Hyperion can take over the strip while they stream.
			</p>
			<label
				><span class="label">While something streams</span>
				<select
					value={Number(a.live_override ?? 0)}
					{disabled}
					onchange={(e) =>
						commands.act(id, 'set_live_override', { live_override: Number(e.currentTarget.value) })}
				>
					<option value={0}>Let it take over</option>
					<option value={1}>Ignore it until it stops</option>
					<option value={2}>Ignore it until the controller restarts</option>
				</select></label
			>
			{#if a.live === true}<button
					class="pill"
					{disabled}
					onclick={() => commands.act(id, 'stop_live')}>Leave live data now</button
				>{/if}
		</section>

		{#if a.has_audio === true}
			<section class="card">
				<div class="head"><h3>Sound</h3></div>
				<label class="check row"
					><span>Listening</span>
					<input
						type="checkbox"
						checked={a.audio_reactive === true}
						{disabled}
						onchange={(e) => commands.act(id, 'set_audio', { audio: e.currentTarget.checked })}
					/></label
				>
				<p class="quiet small">For the effects marked “sound”.</p>
			</section>
		{/if}

		<section class="card">
			<div class="head"><h3>Effect and palette catalogue</h3></div>
			{#each [['controller', 'Read it from the controller', 'Always current. Pickers need the controller to answer.'], ['app', 'Keep a copy in the app', 'Pickers work while it’s offline. Copied again when its firmware changes.']] as [where, label, help] (where)}
				<label class="choice" class:on={catalogStorage(device) === where}>
					<input
						type="radio"
						name={`cat-${id}`}
						checked={catalogStorage(device) === where}
						disabled={!admin}
						onchange={() => setCatalog(where as 'controller' | 'app')}
					/>
					<span><b>{label}</b><br /><span class="quiet small">{help}</span></span>
				</label>
			{/each}
			{#if catalogStorage(device) === 'app'}<button
					class="link"
					onclick={() => wledCatalogs.load(device, String(device.sw_version ?? ''), true)}
					>Copy it again now</button
				>{/if}
			{#if catMsg}<p class="quiet small" role="status">{catMsg}</p>{/if}
		</section>
	</div>

	{#if admin}
		<section class="card">
			<div class="head">
				<h3>Firmware</h3>
				<span class="tag" class:bad={a.firmware_status === 'failed'}
					>{(
						{
							up_to_date: 'Up to date',
							available: 'Update available',
							installing: 'Installing…',
							failed: 'Last update failed',
							no_releases: 'No releases for its build',
							unknown: 'Not checked yet'
						} as Record<string, string>
					)[String(a.firmware_status ?? 'unknown')] ?? String(a.firmware_status)}</span
				>
			</div>
			<div class="facts">
				<div>
					<span class="label">Installed</span><b>{fw?.installed ?? device.sw_version ?? '—'}</b>
				</div>
				<div>
					<span class="label">Newest {channel === 'stable' ? 'stable' : channel}</span><b
						>{a.firmware_latest ?? '—'}</b
					><span class="num small">checked {fmt(a.firmware_checked_at as string)}</span>
				</div>
				<div>
					<span class="label">Build for this board</span><b>{a.firmware_build ?? '—'}</b><span
						class="num small">from {a.firmware_source ?? '—'}</span
					>
				</div>
			</div>
			{#if fw?.failure && a.firmware_status === 'failed'}<p class="warn">{fw.failure}</p>{/if}
			<div class="row-buttons">
				{#if newer}<button
						class="primary"
						disabled={disabled || a.firmware_status === 'installing'}
						onclick={() => install(newer)}>Update to {newer}</button
					>{/if}
				<button class="pill" onclick={checkNow}>Look now</button>
				{#if older.length}
					<select bind:value={pickVersion} aria-label="Another version">
						<option value="">Another version…</option>
						{#each older as o (o.version)}<option value={o.version}
								>{o.version}{o.prerelease ? ' (pre-release)' : ''}</option
							>{/each}
					</select>
					<button
						class="pill"
						disabled={!pickVersion || disabled}
						onclick={() => install(pickVersion)}>Install it</button
					>
				{/if}
				<label class="pill file"
					>Install from a file…<input
						type="file"
						accept=".bin"
						onchange={(e) => {
							const f = e.currentTarget.files?.[0];
							if (f) void fromFile(f);
							e.currentTarget.value = '';
						}}
					/></label
				>
			</div>
			{#if fwMsg}<p class="quiet small" role="status">{fwMsg}</p>{/if}
		</section>

		{#if sections}
			{#if a.has_audio === true && sections.audio}
				<WledSound deviceId={id} audio={sections.audio as never} reload={loadSettings} />
			{/if}
			<WledTimers deviceId={id} {timers} {presets} reload={loadSettings} />
			<WledSetup deviceId={id} {sections} {presets} attrs={a} reload={loadSettings} />
		{:else}
			<section class="card">
				<p class="quiet small">{settingsError || 'Reading its settings…'}</p>
			</section>
		{/if}

		<section class="card">
			<div class="head">
				<h3>Maintenance</h3>
				<span class="num">WLED {device.sw_version ?? ''} · {a.arch ?? ''}</span>
			</div>
			<div class="row-buttons">
				<button class="pill" {disabled} onclick={() => commands.act(id, 'identify')}
					>Identify</button
				>
				<button class="pill" onclick={backupAll}>Back up presets and settings</button>
				<label class="pill file"
					>Restore settings…<input
						type="file"
						accept="application/json,.json"
						onchange={(e) => {
							const f = e.currentTarget.files?.[0];
							if (f) void restoreSettings(f);
							e.currentTarget.value = '';
						}}
					/></label
				>
				{#if a.ip}<a class="pill" href={`http://${a.ip}/`} target="_blank" rel="noreferrer"
						>Open WLED’s own page</a
					>{/if}
				<button class="pill danger" {disabled} onclick={restart}>Restart</button>
			</div>
			{#if maintMsg}<p class="quiet small" role="status">{maintMsg}</p>{/if}
		</section>
	{:else}
		<section class="card">
			<div class="head">
				<h3>Firmware</h3>
				<span class="num">{device.sw_version ?? ''}</span>
			</div>
			<p class="quiet small">
				{a.firmware_update_available === true
					? `${a.firmware_latest} is out — an admin can install it.`
					: 'Up to date as far as homeCore knows.'}
			</p>
			<button class="pill" {disabled} onclick={() => commands.act(id, 'identify')}>Identify</button>
		</section>
	{/if}
</div>

<style>
	.ctl {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
		gap: 14px;
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius);
		background: var(--surface);
		min-width: 0;
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 8px;
		flex-wrap: wrap;
	}
	h3 {
		margin: 0;
		font-size: 16px;
	}
	.label {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
	}
	.num {
		font-family: var(--font-num);
		font-size: 13px;
		color: var(--ink-muted);
	}
	.small {
		font-size: 13px;
	}
	.quiet {
		color: var(--ink-muted);
		margin: 0;
	}
	.warn {
		margin: 0;
		color: var(--alert);
		font-size: 14px;
	}
	.tag {
		font-size: 12px;
		font-weight: 600;
		color: var(--lit-ink);
		background: var(--lit-bg);
		border-radius: 6px;
		padding: 2px 8px;
	}
	.tag.bad {
		color: var(--alert-ink);
		background: var(--alert);
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.two {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 10px;
	}
	input[type='number'],
	select {
		font: inherit;
		min-height: 38px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 8px;
		min-width: 0;
	}
	.check {
		flex-direction: row;
		align-items: center;
		gap: 8px;
		min-height: 40px;
	}
	.check.row {
		justify-content: space-between;
	}
	.check input,
	.choice input {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.chips,
	.groups {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.chips button,
	.groups button {
		font: inherit;
		font-size: 14px;
		min-height: 36px;
		min-width: 36px;
		padding: 0 10px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.chips button[aria-checked='true'],
	.groups button[aria-pressed='true'] {
		border-color: var(--accent);
		background: var(--lit-bg);
		color: var(--lit-ink);
		font-weight: 600;
	}
	.groups .label {
		margin-right: 4px;
	}
	.peers {
		margin: 0;
		padding-left: 18px;
		font-size: 14px;
	}
	.choice {
		flex-direction: row;
		gap: 10px;
		align-items: flex-start;
		padding: 10px;
		border-radius: 11px;
		border: 1px solid var(--line);
	}
	.choice.on {
		border: 2px solid var(--accent);
		background: var(--lit-bg);
	}
	.facts {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
		gap: 12px;
	}
	.facts div {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.facts b {
		font-size: 18px;
	}
	.row-buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.primary {
		font: inherit;
		font-weight: 600;
		min-height: 40px;
		padding: 0 18px;
		border-radius: var(--radius-chip);
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
	}
	.pill {
		font: inherit;
		font-size: 14px;
		min-height: 40px;
		padding: 0 14px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		display: inline-flex;
		align-items: center;
		text-decoration: none;
		align-self: flex-start;
	}
	.pill.danger {
		color: var(--alert);
		border-color: var(--alert-line);
	}
	.file {
		position: relative;
		flex-direction: row;
	}
	.file input {
		position: absolute;
		inset: 0;
		opacity: 0;
	}
	.link {
		font: inherit;
		font-size: 14px;
		border: none;
		background: none;
		color: var(--accent);
		padding: 0;
		align-self: flex-start;
		text-decoration: underline;
	}
	button:disabled {
		opacity: 0.55;
	}
</style>
