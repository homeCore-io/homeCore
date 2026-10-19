<!--
	The WLED plugin's controllers (hc-wled): each with its room, the address
	the plugin reaches it at (changed here), whether changes arrive pushed or
	polled, its hardware and firmware; finding more on the network, adding one
	by address, removing one; and what spans controllers — copying presets,
	backing them all up, updating firmware one at a time; and the plugin's
	own few settings.

	The plugin reads its controllers when it starts, so a change to the list
	restarts it.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import type { CoreDevice } from '#lib/devices.js';
	import { displayName } from '#lib/devices.js';
	import { devices } from '#lib/devices.svelte.js';
	import { rooms } from '#lib/rooms.svelte.js';
	import { roomOf } from '#lib/summary.js';
	import { commands } from '#lib/commands.svelte.js';
	import { session } from '#lib/me.svelte.js';
	import { ask } from '#lib/confirm.svelte.js';
	import { deviceHref } from '#lib/popup.js';
	import { lifecycle, loadConfig, runAction, saveConfig } from '#lib/pluginAdmin.js';
	import { isWled, WLED } from '#lib/wled.js';
	import WledAutoUpdate from '#lib/components/wled/WledAutoUpdate.svelte';

	const list = $derived(
		Object.values(devices.byId)
			.filter((d) => isWled(d))
			.sort((a, b) => displayName(a).localeCompare(displayName(b)))
	);
	const admin = $derived(session.isAdmin);
	let note = $state('');
	let busy = $state(false);

	async function restartPlugin(said: string) {
		note = `${said} Restarting the WLED plugin to take it…`;
		const r = await lifecycle(WLED, 'restart');
		note = r.ok ? `${said} The plugin has restarted.` : r.error;
	}
	async function reply(
		action: string,
		params: Record<string, unknown>
	): Promise<Record<string, unknown> | null> {
		const r = await runAction(WLED, action, params);
		if (!r.ok) {
			note = r.error;
			return null;
		}
		return r.data.kind === 'reply' ? r.data.reply : {};
	}

	// ---- one controller ----
	let editing = $state<string | null>(null);
	let address = $state('');
	async function saveAddress(d: CoreDevice) {
		if (!address.trim()) return;
		if (await reply('update_device', { device_id: d.device_id, host: address.trim() })) {
			editing = null;
			await restartPlugin(`${displayName(d)} is at ${address.trim()} now.`);
		}
	}
	async function remove(d: CoreDevice) {
		if (
			!(await ask({
				title: `Stop looking after ${displayName(d)}? It leaves homeCore, and automations that use it stop working.`,
				go: 'Remove',
				stay: 'Keep it'
			}))
		)
			return;
		if (await reply('remove_device', { device_id: d.device_id }))
			await restartPlugin(`${displayName(d)} is removed.`);
	}
	async function restart(d: CoreDevice) {
		if (
			!(await ask({
				title: `Restart ${displayName(d)}? Its lights go out for a few seconds.`,
				go: 'Restart',
				stay: 'Not now'
			}))
		)
			return;
		void commands.act(d.device_id, 'restart');
	}

	// ---- finding more ----
	type Found = { name?: string; ip?: string; vid?: number; source?: string };
	let found = $state<Found[] | null>(null);
	let dismissed = $state<Set<string>>(new Set());
	const known = (ip?: string) =>
		!!ip && list.some((d) => d.attributes.address === ip || d.attributes.ip === ip);
	async function discover() {
		busy = true;
		note = 'Looking on the network…';
		const r = await reply('discover_devices', {});
		busy = false;
		if (!r) return;
		found = Array.isArray(r.discovered) ? (r.discovered as Found[]) : [];
		const fresh = found.filter((f) => !known(f.ip)).length;
		note = fresh ? '' : 'Nothing new on the network.';
	}
	let newHost = $state('');
	async function add(host: string, name?: string) {
		busy = true;
		const r = await reply('add_device', { host, ...(name ? { name } : {}) });
		busy = false;
		if (r) {
			newHost = '';
			await restartPlugin(`${String(r.name ?? host)} is added.`);
		}
	}

	// ---- across controllers ----
	let copyFrom = $state('');
	let copyTo = $state('');
	async function copy() {
		const from = list.find((d) => d.device_id === copyFrom);
		const to = list.find((d) => d.device_id === copyTo);
		if (!from || !to || from === to) return;
		if (
			!(await ask({
				title: `Replace ${displayName(to)}'s presets with ${displayName(from)}'s?`,
				go: 'Copy',
				stay: 'Not now'
			}))
		)
			return;
		const r = await reply('copy_presets', {
			from_device_id: from.device_id,
			to_device_id: to.device_id
		});
		if (r) note = `Copied ${String(r.copied ?? '')} presets to ${displayName(to)}.`;
	}
	async function backupAll() {
		busy = true;
		const out: Record<string, unknown> = {};
		for (const d of list) {
			note = `Backing up ${displayName(d)}…`;
			const r = await reply('backup', { device_id: d.device_id });
			if (r) out[d.device_id] = r.backup;
		}
		busy = false;
		const a = document.createElement('a');
		a.href = URL.createObjectURL(
			new Blob([JSON.stringify(out, null, 2)], { type: 'application/json' })
		);
		a.download = `wled-backups-${new Date().toISOString().slice(0, 10)}.json`;
		a.click();
		URL.revokeObjectURL(a.href);
		note = `Backed up ${Object.keys(out).length} controller${Object.keys(out).length === 1 ? '' : 's'} — and kept with the plugin too.`;
	}
	const waiting = $derived(list.filter((d) => d.attributes.firmware_update_available === true));
	/** One at a time, and stop at the first that doesn't come back. */
	async function updateAll() {
		if (
			!(await ask({
				title: `Update ${waiting.length} controller${waiting.length === 1 ? '' : 's'}, one at a time? Each goes dark for about a minute; each is backed up first.`,
				go: 'Update',
				stay: 'Not now'
			}))
		)
			return;
		busy = true;
		for (const d of [...waiting]) {
			const v = String(d.attributes.firmware_latest ?? '');
			note = `Updating ${displayName(d)} to ${v}…`;
			await commands.act(d.device_id, 'install_firmware', { version: v });
			const started = Date.now();
			// The install's own status says when it's done.
			let status = 'installing';
			while (Date.now() - started < 300_000) {
				await new Promise((r) => setTimeout(r, 3000));
				status = String(devices.byId[d.device_id]?.attributes.firmware_status ?? '');
				if (Date.now() - started > 8000 && status !== 'installing') break;
			}
			if (status !== 'up_to_date' && status !== 'available') {
				note = `${displayName(d)} didn't come back on ${v} — stopped there.`;
				busy = false;
				return;
			}
		}
		busy = false;
		note = 'Every controller is updated.';
	}
	// ---- the backups the plugin keeps ----
	type Kept = { device_id: string; file: string; bytes: number };
	let kept = $state<Kept[] | null>(null);
	async function listKept() {
		const r = await reply('list_backups', {});
		if (r) kept = Array.isArray(r.backups) ? (r.backups as Kept[]) : [];
	}
	/** "20261018T235224Z-before-16.0.1.json" → "18 Oct 23:52 · before 16.0.1". */
	function keptLabel(file: string): string {
		const m = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z-?(.*)\.json$/.exec(file);
		if (!m) return file;
		const at = new Date(Date.UTC(+m[1]!, +m[2]! - 1, +m[3]!, +m[4]!, +m[5]!, +m[6]!));
		const when = at.toLocaleString([], {
			day: 'numeric',
			month: 'short',
			hour: '2-digit',
			minute: '2-digit',
			hour12: false
		});
		const why = (m[7] ?? '').replace(/-/g, ' ').replace(/^before (\d)/, 'before $1');
		return why ? `${when} · ${why}` : when;
	}
	const whose = (k: Kept) => {
		const d = devices.byId[k.device_id];
		return d ? displayName(d) : k.device_id;
	};
	async function restoreKept(k: Kept, what: 'presets' | 'settings') {
		if (
			!(await ask({
				title:
					what === 'presets'
						? `Put ${k.file}'s presets back on ${whose(k)}? Its presets now are replaced — and kept as a backup first.`
						: `Put ${k.file}'s settings back on ${whose(k)}? Its Wi-Fi and other secrets stay as they are; some settings take effect after a restart.`,
				go: 'Restore',
				stay: 'Not now'
			}))
		)
			return;
		busy = true;
		const r = await reply('restore', {
			device_id: k.device_id,
			file: k.file,
			presets: what === 'presets',
			settings: what === 'settings'
		});
		busy = false;
		if (r) {
			note = `${whose(k)}'s ${what} are restored from ${k.file}.`;
			await listKept();
		}
	}
	async function downloadKept(k: Kept) {
		const r = await reply('get_backup', { device_id: k.device_id, file: k.file });
		if (!r) return;
		const a = document.createElement('a');
		a.href = URL.createObjectURL(
			new Blob([JSON.stringify(r.backup, null, 2)], { type: 'application/json' })
		);
		a.download = `${k.device_id}-${k.file}`;
		a.click();
		URL.revokeObjectURL(a.href);
	}

	// ---- its settings ----
	let catalog = $state('controller');
	let poll = $state(30);
	let channel = $state('stable');
	let others = $state('');
	let loaded = $state('');
	const current = $derived(JSON.stringify([catalog, poll, channel, others.trim()]));
	onMount(async () => {
		if (!admin) return;
		const r = await loadConfig(WLED);
		const w = (r.ok ? r.data.config?.wled : undefined) as Record<string, unknown> | undefined;
		if (!w) return;
		catalog = w.catalog === 'app' ? 'app' : 'controller';
		poll = typeof w.poll_interval_secs === 'number' ? w.poll_interval_secs : 30;
		channel = typeof w.firmware_channel === 'string' ? w.firmware_channel : 'stable';
		others = Array.isArray(w.discovery_hosts) ? w.discovery_hosts.join('\n') : '';
		loaded = current;
	});
	async function saveSettings() {
		busy = true;
		const r = await saveConfig(WLED, {
			patch: {
				wled: {
					catalog,
					poll_interval_secs: Math.max(5, Math.round(poll)),
					firmware_channel: channel,
					discovery_hosts: others
						.split(/[\s,]+/)
						.map((h) => h.trim())
						.filter(Boolean)
				}
			}
		});
		busy = false;
		if (!r.ok) return void (note = r.error);
		loaded = current;
		await restartPlugin('Settings saved.');
	}

	const signal = (d: CoreDevice) =>
		typeof d.attributes.wifi_signal_pct === 'number' ? `${d.attributes.wifi_signal_pct}%` : '—';
</script>

<section class="card wled">
	<div class="head">
		<h2>Controllers</h2>
		{#if admin}
			<div class="tools">
				<button class="primary" disabled={busy} onclick={discover}>Look for controllers</button>
			</div>
		{/if}
	</div>
	<div class="scroll">
		<table>
			<thead>
				<tr
					><th>Name and room</th><th>Address</th><th>State</th><th>LEDs · signal · firmware</th><th
					></th></tr
				>
			</thead>
			<tbody>
				{#each list as d (d.device_id)}
					{@const a = d.attributes}
					<tr>
						<td>
							<a href={deviceHref(d.device_id, page.url)}>{displayName(d)}</a>
							<span class="sub"
								>{roomOf(d, rooms.list)?.name ?? 'Not in a room'} · {d.device_id}</span
							>
						</td>
						<td>
							{#if editing === d.device_id}
								<form class="inline" onsubmit={(e) => (e.preventDefault(), saveAddress(d))}>
									<input bind:value={address} aria-label={`Address of ${displayName(d)}`} />
									<button class="pill">Save</button>
									<button type="button" class="pill" onclick={() => (editing = null)}>Cancel</button
									>
								</form>
							{:else}
								<span class="mono">{a.address ?? a.ip ?? '—'}</span>
								{#if admin}<button
										class="link"
										onclick={() => (
											(editing = d.device_id),
											(address = String(a.address ?? a.ip ?? ''))
										)}>change</button
									>{/if}
							{/if}
						</td>
						<td>
							<span
								class="dot"
								class:ok={d.available && a.connection === 'push'}
								class:warn={d.available && a.connection !== 'push'}
								class:bad={!d.available}
							></span>
							{!d.available ? 'Not answering' : a.connection === 'push' ? 'Live' : 'Polling'}
						</td>
						<td class="mono">
							{a.led_count ?? '—'} · {signal(d)} · {d.sw_version ??
								'—'}{a.firmware_update_available === true ? ` → ${a.firmware_latest}` : ''}
						</td>
						<td class="acts">
							<button
								class="pill"
								disabled={!d.available}
								onclick={() => commands.act(d.device_id, 'identify')}>Identify</button
							>
							{#if admin}
								<button class="pill" disabled={!d.available} onclick={() => restart(d)}
									>Restart</button
								>
								<button class="pill danger" onclick={() => remove(d)}>Remove</button>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
	{#if list.length === 0}<p class="sub">No controllers yet.</p>{/if}

	{#if found}
		{#each found.filter((f) => !known(f.ip) && !dismissed.has(f.ip ?? '')) as f (f.ip)}
			<div class="found">
				<span
					>Found on the network, not added: <b>{f.name ?? 'a WLED'}</b>
					<span class="mono">{f.ip}</span></span
				>
				<button class="pill" disabled={busy} onclick={() => f.ip && add(f.ip, f.name)}
					>Add it</button
				>
				<button class="pill" onclick={() => (dismissed = new Set([...dismissed, f.ip ?? '']))}
					>Not this one</button
				>
			</div>
		{/each}
	{/if}

	{#if admin}
		<form class="inline" onsubmit={(e) => (e.preventDefault(), add(newHost.trim()))}>
			<input
				bind:value={newHost}
				placeholder="10.0.10.80 or wled-garage.local"
				aria-label="A controller's address"
			/>
			<button class="pill" disabled={busy || !newHost.trim()}>Add one by address</button>
		</form>
	{/if}
</section>

{#if admin}
	<div class="pair">
		<section class="card wled">
			<h2>Across controllers</h2>
			{#if list.length > 1}
				<div class="row copy">
					<span>Copy presets</span>
					<select bind:value={copyFrom} aria-label="From">
						<option value="">from…</option>
						{#each list as d (d.device_id)}<option value={d.device_id}>{displayName(d)}</option
							>{/each}
					</select>
					<select bind:value={copyTo} aria-label="To">
						<option value="">to…</option>
						{#each list.filter((d) => d.device_id !== copyFrom) as d (d.device_id)}<option
								value={d.device_id}>{displayName(d)}</option
							>{/each}
					</select>
					<button class="pill" disabled={!copyFrom || !copyTo || busy} onclick={copy}>Copy</button>
				</div>
			{/if}
			<div class="row">
				<span
					>Back up {list.length === 1 ? 'the controller’s' : 'every controller’s'} presets and settings</span
				>
				<button class="pill" disabled={busy || !list.length} onclick={backupAll}>Back up</button>
			</div>
			<div class="row">
				<span
					>Firmware: {waiting.length
						? `${waiting.length} can be updated`
						: list.length
							? 'all up to date'
							: '—'}</span
				>
				{#if waiting.length}<button class="pill" disabled={busy} onclick={updateAll}
						>Update all…</button
					>{/if}
			</div>
			<p class="sub">
				Update all goes one controller at a time and stops at the first that doesn’t come back.
			</p>
			<WledAutoUpdate />
			<div class="row">
				<span>Backups kept with the plugin</span>
				<button class="pill" disabled={busy} onclick={listKept}
					>{kept ? 'Look again' : 'Show them'}</button
				>
			</div>
			{#if kept}
				{#if kept.length === 0}<p class="sub">None yet.</p>{:else}
					<ul class="kept">
						{#each kept as k (k.device_id + k.file)}
							<li>
								<span title={k.file}
									>{#if list.length > 1}<b>{whose(k)}</b>{/if}<span class="sub"
										>{keptLabel(k.file)}</span
									></span
								>
								<button class="pill" disabled={busy} onclick={() => restoreKept(k, 'presets')}
									>Presets back</button
								>
								<button class="pill" disabled={busy} onclick={() => restoreKept(k, 'settings')}
									>Settings back</button
								>
								<button class="pill" onclick={() => downloadKept(k)}>Download</button>
							</li>
						{/each}
					</ul>
				{/if}
			{/if}
		</section>

		<section class="card wled">
			<h2>Settings</h2>
			<label class="field">
				<span class="lbl">Effect and palette catalogue, for new controllers</span>
				<select bind:value={catalog}>
					<option value="controller">Read from the controller</option>
					<option value="app">Keep a copy in the app</option>
				</select>
			</label>
			<label class="field">
				<span class="lbl">When a controller can’t push, read it every (seconds)</span>
				<input type="number" min="5" bind:value={poll} />
			</label>
			<label class="field">
				<span class="lbl">Firmware to offer</span>
				<select bind:value={channel}>
					<option value="stable">Stable releases</option>
					<option value="beta">Stable and betas</option>
					<option value="nightly">Nightly builds too</option>
				</select>
			</label>
			<label class="field">
				<span class="lbl">Controllers on other networks</span>
				<textarea
					bind:value={others}
					rows="2"
					placeholder="Addresses, one per line — only for WLEDs on another VLAN"></textarea>
			</label>
			<div class="row">
				<span class="sub">Saving restarts the plugin.</span>
				<button class="pill" disabled={busy || !loaded || current === loaded} onclick={saveSettings}
					>Save</button
				>
			</div>
		</section>
	</div>
{/if}
{#if note}<p class="note" role="status">{note}</p>{/if}

<style>
	.wled {
		display: flex;
		flex-direction: column;
		gap: 12px;
		min-width: 0;
		padding: var(--space-4);
		border-radius: var(--radius);
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
	}
	h2 {
		margin: 0;
		font-size: 18px;
	}
	.scroll {
		overflow-x: auto;
	}
	table {
		width: 100%;
		border-collapse: collapse;
		min-width: 760px;
		font-size: 14px;
	}
	th {
		text-align: left;
		font-size: 12px;
		color: var(--ink-muted);
		font-weight: 600;
		padding: 6px 8px;
	}
	td {
		padding: 8px;
		border-top: 1px solid var(--card-line);
		vertical-align: middle;
	}
	td a {
		font-weight: 600;
	}
	.sub {
		display: block;
		font-size: 12px;
		color: var(--ink-muted);
		margin: 0;
	}
	.mono {
		font-family: var(--font-num);
		font-size: 13px;
	}
	.dot {
		display: inline-block;
		width: 10px;
		height: 10px;
		border-radius: 50%;
		margin-right: 6px;
		background: var(--line);
	}
	.dot.ok {
		background: #2f8f4e;
	}
	.dot.warn {
		background: var(--track-on);
	}
	.dot.bad {
		background: var(--alert);
	}
	.acts {
		white-space: nowrap;
	}
	.inline,
	.row {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.row span {
		flex: 1 1 240px;
	}
	.row.copy span {
		flex: 0 0 auto;
	}
	.row.copy select {
		flex: 1 1 140px;
	}
	.kept li > span b {
		display: block;
	}
	.kept {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 260px;
		overflow: auto;
	}
	.kept li {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		padding: 6px 0;
		border-top: 1px solid var(--card-line);
	}
	.kept li > span {
		flex: 1 1 200px;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.pair {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 380px), 1fr));
		gap: var(--space-4);
		align-items: start;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.lbl {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
	}
	.ok {
		color: #2f8f4e;
	}
	textarea {
		font: inherit;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 8px 10px;
		resize: vertical;
	}
	input,
	select {
		font: inherit;
		min-height: 38px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 10px;
		min-width: 0;
	}
	.inline input {
		flex: 1 1 220px;
	}
	.found {
		display: flex;
		gap: 10px;
		align-items: center;
		flex-wrap: wrap;
		padding: 10px;
		border: 1px dashed var(--track-on);
		border-radius: 11px;
	}
	.found span {
		flex: 1 1 260px;
	}
	.primary {
		font: inherit;
		font-weight: 600;
		min-height: 40px;
		padding: 0 16px;
		border-radius: var(--radius-chip);
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
	}
	.pill {
		font: inherit;
		font-size: 13px;
		min-height: 34px;
		padding: 0 12px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.pill.danger {
		color: var(--alert);
		border-color: var(--alert-line);
	}
	.link {
		font: inherit;
		font-size: 13px;
		border: none;
		background: none;
		color: var(--accent);
		text-decoration: underline;
		padding: 0 4px;
	}
	.note {
		margin: 0;
		font-size: 14px;
		color: var(--ink-muted);
	}
	button:disabled {
		opacity: 0.55;
	}
</style>
