<svelte:options customElement={{ tag: 'hc-ecowitt-page', props: { host: { type: 'Object' } } }} />

<!-- hc-ecowitt's page: is the gateway reporting, and where to; the weather
     now; every sensor with its battery; what the gateway itself hears; and
     the plugin's settings. -->
<script>
	import Now from './lib/Now.svelte';
	import { adopt, ago, battery, kindOf, reading, stale } from './lib/weather.js';
	import { follow } from './lib/units.svelte.js';

	/** @type {{ host: import('../../../../sdk/ui/index.js').Host }} */
	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));
	$effect(() => follow(hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 5000);
		return () => clearInterval(t);
	});

	const admin = $derived(hc?.me.isAdmin ?? false);
	const gateway = $derived(devices.find((d) => d.device_type === 'gateway' && d.available) ?? devices.find((d) => d.device_type === 'gateway'));
	const g = $derived(gateway?.attributes ?? {});
	const sensors = $derived(devices.filter((d) => d.device_type !== 'gateway'));
	const newest = $derived(
		sensors.map((d) => d.last_seen).filter(Boolean).sort().at(-1)
	);
	const reporting = $derived(newest !== undefined && !stale(newest, now));
	const low = $derived(sensors.filter((d) => d.attributes.battery_low === true));

	// ---- the plugin's settings (admins) ----
	let cfg = $state(null);
	let msg = $state('');
	let busy = $state(false);
	let form = $state({ listen_port: 8888, bind_addr: '127.0.0.1', allowed: '', gateway_ip: '', poll: 60 });
	$effect(() => {
		if (!hc || !admin) return;
		void hc.config.get().then((r) => {
			if (!r.ok) return;
			const e = r.data.ecowitt ?? {};
			cfg = e;
			form = {
				listen_port: e.listen_port ?? 8888,
				bind_addr: e.bind_addr ?? '127.0.0.1',
				allowed: (e.allowed_source_ips ?? []).join('\n'),
				gateway_ip: e.gateway_ip ?? '',
				poll: e.poll_interval_secs ?? 60
			};
		});
	});
	async function save() {
		const ok = await hc.ui.confirm({
			title: 'Save and restart the Ecowitt plugin?',
			body: 'It reads its settings when it starts. Readings pause for a few seconds.',
			go: 'Save and restart'
		});
		if (!ok) return;
		busy = true;
		const w = await hc.config.patch('ecowitt', {
			listen_port: Number(form.listen_port),
			bind_addr: form.bind_addr.trim(),
			allowed_source_ips: form.allowed.split(/[\s,]+/).filter(Boolean),
			gateway_ip: form.gateway_ip.trim(),
			poll_interval_secs: Number(form.poll)
		});
		const r = w.ok ? await hc.config.restartPlugin() : w;
		busy = false;
		msg = r.ok ? 'Saved; the plugin is restarting.' : r.error;
	}

	// The gateway's upload, against where the plugin listens.
	const uploads = $derived(
		g['customserver.server']
			? `${g['customserver.server']}:${g['customserver.port']}${g['customserver.path'] ?? ''}`
			: ''
	);
	const mismatch = $derived(
		cfg && g['customserver.port'] && String(g['customserver.port']) !== String(cfg.listen_port ?? 8888)
	);
	async function pointHere() {
		const ok = await hc.ui.confirm({
			title: 'Point the gateway’s uploads at homeCore?',
			body: 'It sets the gateway’s “Customized” upload to this plugin’s receiver, every 60 seconds, in Ecowitt’s format.',
			go: 'Point it here'
		});
		if (!ok) return;
		busy = true;
		const r = await hc.action('set_custom_server', {});
		busy = false;
		msg = r.ok ? 'The gateway now uploads here.' : r.error;
	}

	// ---- what the gateway hears ----
	let heard = $state(null);
	let hearing = $state(false);
	async function ask() {
		hearing = true;
		const r = await hc.action('refresh_sensors', {});
		hearing = false;
		if (!r.ok) return void (msg = r.error);
		const pages = Object.values(r.data.sensors ?? {}).flatMap((p) => (Array.isArray(p) ? p : []));
		heard = pages.filter((s) => s.idst === '1' && !/^F{7}[EF]$/i.test(s.id ?? ''));
	}
	const bars = (s) => Math.max(0, Math.min(4, Number(s) || 0));
</script>

<div class="page">
	<section class="hc-card gw">
		<div class="hc-head">
			<h2>
				<span class="hc-dot" class:good={reporting} class:bad={!reporting}></span>
				{reporting ? 'Reporting' : newest ? 'Not reporting' : 'Waiting for the gateway'}
			</h2>
			<span class="hc-quiet">{newest ? `last reading ${ago(newest, now)}` : 'no readings yet'}</span>
		</div>
		{#if gateway}
			<div class="facts">
				<span><span class="hc-label">Gateway</span> {g.model ?? '—'}</span>
				<span><span class="hc-label">Firmware</span> <span class="hc-num">{g.firmware ?? '—'}</span>
					{#if g.update_available === true}<span class="hc-tag warn">update available</span>{/if}</span>
				<span><span class="hc-label">Address</span> <span class="hc-num">{g.ip ?? '—'}</span></span>
				{#if uploads}<span
						><span class="hc-label">Uploads to</span>
						<span class="hc-num">{uploads}</span> every {g['customserver.interval'] ?? '?'} s{g['customserver.enable'] === false ? ' — turned off' : ''}</span
					>{/if}
			</div>
			{#if mismatch}
				<p class="warn">
					The gateway uploads to port {g['customserver.port']}, but this plugin listens on {cfg.listen_port}.
				</p>
			{/if}
			{#if admin}
				<div class="hc-row">
					<button class="hc-pill" disabled={busy} onclick={pointHere}>Point its uploads here</button>
					<button class="hc-pill" onclick={() => hc.ui.openDevice(gateway.device_id)}>Gateway details</button>
				</div>
			{/if}
		{:else}
			<p class="hc-quiet">
				The gateway itself shows up once the plugin can reach it — set its address below.
			</p>
		{/if}
	</section>

	<Now devices={sensors} />

	<section class="hc-card">
		<div class="hc-head">
			<h3>Sensors <span class="hc-quiet">· {sensors.length}</span></h3>
			{#if low.length}<span class="hc-tag bad"
					>{low.length} batter{low.length === 1 ? 'y' : 'ies'} low</span
				>{:else if sensors.length}<span class="hc-tag good">batteries fine</span>{/if}
		</div>
		<div class="scroll">
			<table class="hc-table">
				<thead>
					<tr><th>Sensor</th><th>Reading</th><th>Battery</th><th>Heard</th></tr>
				</thead>
				<tbody>
					{#each sensors as d (d.device_id)}
						{@const b = battery(d.attributes)}
						<tr>
							<td>
								<button class="link" onclick={() => hc.ui.openDevice(d.device_id)}>{hc.devices.label(d)}</button>
								<span class="sub">{kindOf(d.device_type)}{d.room ? ` · ${d.room}` : ''}</span>
							</td>
							<td class="hc-num">{reading(d)}</td>
							<td><span class="hc-tag" class:good={b.tone === 'good'} class:bad={b.tone === 'bad'}>{b.text}</span></td>
							<td class:late={stale(d.last_seen, now)}>{ago(d.last_seen, now)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</section>

	<div class="hc-grid">
		<section class="hc-card">
			<div class="hc-head">
				<h3>What the gateway hears</h3>
				<button class="hc-pill" disabled={hearing} onclick={ask}>{heard ? 'Ask again' : 'Ask the gateway'}</button>
			</div>
			<p class="hc-quiet">Every sensor paired with it, with its signal — the place to look when one goes quiet.</p>
			{#if heard}
				<table class="hc-table">
					<thead><tr><th>Sensor</th><th>Signal</th><th>Battery</th><th>Id</th></tr></thead>
					<tbody>
						{#each heard as s (s.id + s.type)}
							<tr>
								<td>{s.name}</td>
								<td>
									<span class="bars" aria-label={`${bars(s.signal)} of 4`}>
										{#each [1, 2, 3, 4] as n (n)}<i class:on={bars(s.signal) >= n} style:height={`${4 + n * 3}px`}></i>{/each}
									</span>
									{#if s.rssi && s.rssi !== '--'}<span class="hc-num hc-quiet"> {s.rssi} dBm</span>{/if}
								</td>
								<td class="hc-num">{s.batt}</td>
								<td class="hc-num hc-quiet">{s.id}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			{/if}
		</section>

		{#if admin}
			<section class="hc-card">
				<h3>Settings</h3>
				<div class="two">
					<label class="hc-field"><span class="hc-label">Receive on port</span>
						<input type="number" min="1" max="65535" bind:value={form.listen_port} /></label>
					<label class="hc-field"><span class="hc-label">Listen on</span>
						<select bind:value={form.bind_addr}>
							<option value="0.0.0.0">The network (0.0.0.0)</option>
							<option value="127.0.0.1">This machine only</option>
							{#if !['0.0.0.0', '127.0.0.1'].includes(form.bind_addr)}<option value={form.bind_addr}>{form.bind_addr}</option>{/if}
						</select></label>
				</div>
				<label class="hc-field"><span class="hc-label">Accept readings only from</span>
					<textarea rows="2" bind:value={form.allowed} placeholder="The gateway’s address — Ecowitt uploads have no password"></textarea></label>
				<div class="two">
					<label class="hc-field"><span class="hc-label">Gateway address</span>
						<input bind:value={form.gateway_ip} placeholder="10.0.10.21" /></label>
					<label class="hc-field"><span class="hc-label">Also read it every (s, 0 for never)</span>
						<input type="number" min="0" bind:value={form.poll} /></label>
				</div>
				{#if form.bind_addr === '0.0.0.0' && !form.allowed.trim()}
					<p class="warn">Anyone on the network could send readings. Name the gateway above.</p>
				{/if}
				<div class="hc-row">
					<button class="hc-pill go" disabled={busy} onclick={save}>Save</button>
				</div>
			</section>
		{/if}
	</div>
	{#if msg}<p class="hc-quiet" role="status">{msg}</p>{/if}
	<p class="hc-quiet foot">This page comes with the Ecowitt plugin, in its own release.</p>
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	h2,
	h3 {
		margin: 0;
		font-size: 17px;
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.facts {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 22px;
		font-size: 14px;
	}
	.warn {
		margin: 0;
		color: var(--alert);
		font-size: 14px;
	}
	.scroll {
		overflow-x: auto;
	}
	.scroll table {
		min-width: 560px;
	}
	.link {
		font: inherit;
		font-weight: 600;
		border: none;
		background: none;
		padding: 0;
		color: var(--ink);
		cursor: pointer;
		text-decoration: underline;
		text-decoration-color: var(--line);
	}
	.sub {
		display: block;
		font-size: 12px;
		color: var(--ink-muted);
	}
	.late {
		color: var(--alert);
	}
	.bars {
		display: inline-flex;
		align-items: flex-end;
		gap: 2px;
		height: 16px;
	}
	.bars i {
		width: 4px;
		border-radius: 1px;
		background: var(--line);
	}
	.bars i.on {
		background: #2f8f4e;
	}
	.two {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 160px), 1fr));
		gap: 12px;
	}
	textarea {
		padding: 8px 10px !important;
		resize: vertical;
	}
	.foot {
		font-size: 12px;
	}
</style>
