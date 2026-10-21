<!-- A device's health: whether it answers and when it was last heard, its
     signal and messages, its route, security, power, node and firmware; Ping,
     rebuild its routes, interview it again. A device that stops answering
     offers removing it from the network, or replacing it. -->
<script>
	import AddDevice from './AddDevice.svelte';
	import { ago, failed, num, powerDetail, routeWords, securityWords, SIGNAL_WORDS, STATUS_WORDS } from './zwave.js';

	let { hc, deviceId, node, name = '', admin = false, onchanged } = $props();

	let pinged = $state('');
	let busy = $state('');
	let error = $state('');
	let firmware = $state('');
	let replacing = $state(false);
	let tick = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (tick = Date.now()), 30000);
		return () => clearInterval(t);
	});

	const st = $derived(node?.statistics ?? {});
	const dead = $derived(node?.status === 'dead');
	const statusWord = $derived(node ? STATUS_WORDS[node.status] ?? 'Not heard yet' : '—');
	const heard = $derived(node?.last_seen ? `heard ${ago(node.last_seen, tick)}` : node?.status === 'asleep' ? 'asleep until it next wakes' : '');
	const signalLine = $derived(
		[num(st.rssi) !== undefined ? `${st.rssi} dBm` : '', num(st.rtt_ms) !== undefined ? `${Math.round(st.rtt_ms)} ms round trip` : ''].filter(Boolean).join(' · ') || 'not measured yet'
	);

	async function run(id, word, extra = {}) {
		busy = id;
		error = '';
		const r = await hc.action(id, { device_id: deviceId, ...extra });
		busy = '';
		const err = failed(r);
		if (err) error = err;
		else onchanged?.();
		return err ? null : r.data;
	}

	async function ping() {
		pinged = '';
		const d = await run('ping_node', 'Ping');
		if (d) pinged = d.responded ? `Answered in ${d.ms} ms.` : 'It didn’t answer.';
	}

	async function checkFirmware() {
		firmware = 'Asking…';
		const r = await hc.action('check_firmware', { device_id: deviceId });
		const err = failed(r);
		if (err) firmware = `Couldn’t check: ${err}`;
		else {
			const u = r.data.updates ?? [];
			firmware = u.length ? `Version ${u[0].version} is available${u[0].changelog ? ` — ${u[0].changelog}` : ''}. Install it from Z-Wave JS UI.` : 'It’s up to date.';
		}
	}

	async function removeFailed() {
		const ok = await hc.ui.confirm({
			title: `Remove ${name || 'this device'} from the network?`,
			body: 'For a device that is broken or gone. If it comes back to life it will need adding again.',
			lost: ['Its room', 'Its place in automations and on dashboards'],
			go: 'Remove it',
			danger: true
		});
		if (ok) await run('remove_failed_node', 'Remove');
	}

	async function reinterview() {
		const ok = await hc.ui.confirm({
			title: 'Ask it what it can do again?',
			body: 'It is interviewed again, as when it was added. A battery device answers when it next wakes.',
			go: 'Ask again'
		});
		if (ok) await run('reinterview_node', 'Interview');
	}
</script>

{#if node}
	<div class="health">
		<div class="tiles">
			<div class="tile">
				<span class="hc-label">Status</span>
				<span class="v" class:good={!dead && node.status !== 'unknown'} class:bad={dead}>{statusWord}</span>
				<span class="sub">{heard}</span>
			</div>
			<div class="tile">
				<span class="hc-label">Signal</span>
				<span class="v" class:bad={node.signal === 'poor'}>{SIGNAL_WORDS[node.signal] ?? '—'}</span>
				<span class="sub">{signalLine}</span>
			</div>
			<div class="tile">
				<span class="hc-label">Messages</span>
				<span class="v">{(st.sent ?? 0).toLocaleString()} sent</span>
				<span class="sub">{st.failed ? `${st.failed.toLocaleString()} didn’t arrive` : 'all arrived'}</span>
			</div>
		</div>
		<dl class="rows">
			<dt>Route</dt>
			<dd>{routeWords(node.route)}</dd>
			<dt>Security</dt>
			<dd>{securityWords(node.security)}</dd>
			<dt>Power</dt>
			<dd>{powerDetail(node)}</dd>
			<dt>Node</dt>
			<dd class="hc-num">{node.node_id}</dd>
			<dt>Firmware</dt>
			<dd>{node.firmware ?? '—'} · <button class="link" onclick={checkFirmware}>check for an update</button>{#if firmware}<span class="fw">{firmware}</span>{/if}</dd>
		</dl>
		<div class="acts">
			<div class="hc-row">
				<button class="hc-pill" disabled={!!busy} onclick={ping}>{busy === 'ping_node' ? 'Pinging…' : 'Ping'}</button>
				{#if admin && !dead}
					<button class="hc-pill" disabled={!!busy} onclick={() => run('rebuild_node_routes', 'Rebuild')}>{busy === 'rebuild_node_routes' ? 'Rebuilding…' : 'Rebuild its routes'}</button>
					<button class="hc-pill" disabled={!!busy} onclick={reinterview}>Ask it what it can do again</button>
				{/if}
				{#if admin && dead}
					<button class="hc-pill danger" disabled={!!busy} onclick={removeFailed}>Remove it from the network</button>
					<button class="hc-pill" disabled={!!busy} onclick={() => (replacing = true)}>Replace it</button>
				{/if}
			</div>
			{#if pinged}<span class="said" class:bad={pinged.startsWith('It didn')}>{pinged}</span>{/if}
			{#if error}<span class="err" role="alert">{error}</span>{/if}
			{#if busy === 'rebuild_node_routes'}<span class="hc-quiet">This can take a minute.</span>{/if}
			<span class="hc-quiet note"
				>{dead
					? 'It has stopped answering. Ping it once it has power again. Gone for good? Remove it from the network — or replace it with a new one, which keeps its name, room and automations.'
					: 'A device that stops answering shows here as “Not answering”, with Ping and, if it’s gone for good, “Remove it from the network”.'}</span
			>
		</div>
	</div>
	{#if replacing}
		<AddDevice {hc} replace={deviceId} replaceName={name} onclose={() => ((replacing = false), onchanged?.())} />
	{/if}
{:else}
	<p class="hc-quiet">Asking Z-Wave JS about it…</p>
{/if}

<style>
	.health {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 12px;
		border-radius: 12px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		min-width: 0;
	}
	.v {
		font-weight: 600;
	}
	.v.good {
		color: light-dark(#2f5a27, #8fd19e);
	}
	.v.bad {
		color: var(--alert);
	}
	.sub {
		font-size: 12px;
		color: var(--ink-muted);
		overflow-wrap: anywhere;
	}
	.rows {
		display: grid;
		grid-template-columns: 110px minmax(0, 1fr);
		row-gap: 8px;
		margin: 0;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		font-size: 14px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
		min-width: 0;
	}
	.fw {
		display: block;
		margin-top: 4px;
		font-size: 13px;
		color: var(--ink-muted);
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font: inherit;
		text-decoration: underline;
	}
	.acts {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.acts .hc-pill {
		min-height: 40px;
	}
	.said {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.said.bad,
	.err {
		color: var(--alert);
	}
	.note {
		font-size: 13px;
	}
	@media (max-width: 420px) {
		.tiles {
			grid-template-columns: 1fr 1fr;
		}
		.tiles .tile:last-child {
			grid-column: span 2;
		}
	}
</style>
