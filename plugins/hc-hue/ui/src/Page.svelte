<svelte:options customElement={{ tag: 'hc-hue-page', props: { host: { type: 'Object' } } }} />

<!-- Hue's plugin page: its bridges and adding one, its rooms and zones (and
     which are devices of their own), what needs a look, and its main
     settings. -->
<script>
	import Pair from './lib/Pair.svelte';
	import { adopt, num } from './lib/hue.js';

	let { host: hc } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	// Bridges this plugin is running: a refresh has counted their lights, or
	// they are waiting to be paired.
	const bridges = $derived(
		devices.filter(
			(d) => d.device_type === 'bridge' && (d.attributes.light_count !== undefined || d.attributes.integration_state === 'auth_required')
		)
	);
	const count = (t) => devices.filter((d) => d.device_type === t).length;
	const sensors = $derived(devices.filter((d) => ['motion_sensor', 'button', 'contact_sensor'].includes(d.device_type ?? '')).length);
	const admin = $derived(hc?.me.isAdmin || hc?.me.can('plugins:write'));

	// Rooms and zones, published as devices or not, from the plugin.
	let rooms = $state([]);
	let config = $state(null);
	async function load() {
		const r = await hc.query('rooms');
		if (r.ok) rooms = r.data.rooms ?? [];
		if (admin) {
			const c = await hc.config.get();
			if (c.ok) config = c.data;
		}
	}
	$effect(() => {
		if (hc) void load();
	});
	const hue = $derived(config?.hue ?? {});
	const slug = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_+|_+$/g, '');
	const selector = (r) => `${r.kind}:${slug(r.name)}`;
	const published = (r) => r.device_id !== null && r.device_id !== undefined;

	let saving = $state('');
	let error = $state('');
	async function setting(values, what) {
		saving = what;
		error = '';
		const r = await hc.config.patch('hue', values);
		if (r.ok) {
			const back = await hc.config.restartPlugin();
			if (!back.ok) error = back.error;
		} else error = r.error;
		saving = '';
		setTimeout(load, 4000);
	}
	function toggleRoom(r) {
		const now = Array.isArray(hue.publish_grouped_lights_for) ? hue.publish_grouped_lights_for : [];
		const sel = selector(r);
		const next = published(r) ? now.filter((s) => s !== sel && slug(s.replace(/^(room|zone):/, '')) !== slug(r.name)) : [...now, sel];
		setting({ publish_grouped_lights_for: next }, r.id);
	}

	let refreshing = $state(false);
	async function refresh() {
		refreshing = true;
		const r = await hc.action('refresh_devices');
		if (!r.ok) error = r.error;
		setTimeout(() => {
			refreshing = false;
			void load();
		}, 3000);
	}

	async function remove(b) {
		const ok = await hc.ui.confirm({
			title: `Remove ${hc.devices.label(b)}?`,
			body: 'Its lights, rooms, scenes and sensors leave homeCore. Automations that use them stop working. You can add the bridge again later.',
			go: 'Remove',
			danger: true
		});
		if (!ok) return;
		const r = await hc.action('unpair_bridge', { bridge_id: b.attributes.bridge_id });
		if (!r.ok) error = r.error;
	}

	let adding = $state(false);
	const attention = $derived(
		devices
			.filter((d) => d.device_type !== 'bridge' && d.device_type !== 'scene')
			.flatMap((d) => {
				const out = [];
				if (!d.available && d.device_type === 'light') out.push({ d, why: 'not answering — switched off at the wall?' });
				const bat = num(d.attributes.battery_pct);
				if (bat !== undefined && bat <= 15) out.push({ d, why: `battery ${Math.round(bat)} %` });
				if (d.attributes.update_state === 'ready_to_install') out.push({ d, why: 'an update is ready in the Hue app' });
				return out;
			})
	);
	const conn = (b) =>
		!b.available || b.attributes.online === false
			? { tone: 'bad', word: 'Not answering' }
			: b.attributes.integration_state === 'auth_required'
				? { tone: 'warn', word: 'Needs pairing' }
				: b.attributes.live
					? { tone: 'good', word: 'Live' }
					: { tone: 'warn', word: `Checked every ${hue.resync_interval_secs ?? 60} s` };
</script>

<div class="page">
	<header class="hc-head top">
		<span class="hc-quiet hc-num"
			>{bridges.length} bridge{bridges.length === 1 ? '' : 's'} · {count('light')} lights · {sensors} sensors and switches · {count('scene')} scenes</span
		>
		{#if admin}
			<span class="hc-row">
				<button class="hc-pill go" onclick={() => (adding = !adding)}>{adding ? 'Close' : 'Add a bridge'}</button>
				<button class="hc-pill" onclick={refresh} disabled={refreshing}>{refreshing ? 'Refreshing…' : 'Refresh everything'}</button>
			</span>
		{/if}
	</header>
	{#if error}<p class="err" role="alert">{error}</p>{/if}

	{#if adding}
		<Pair {hc} ondone={() => {
			adding = false;
			void load();
		}} />
	{/if}

	<section class="hc-card">
		<div class="hc-head"><h2>Bridges</h2><span class="hc-quiet">live = changes arrive the moment they happen</span></div>
		{#if bridges.length}
			<div class="scroll">
				<table class="hc-table">
					<thead><tr><th>Bridge</th><th>Address</th><th>Connection</th><th>Has</th><th></th></tr></thead>
					<tbody>
						{#each bridges as b (b.device_id)}
							{@const c = conn(b)}
							<tr>
								<td
									><b>{hc.devices.label(b)}</b><br /><span class="hc-quiet hc-num"
										>{b.attributes.model_id === 'BSB002' ? 'Bridge v2' : b.attributes.model_id === 'BSB003' ? 'Bridge Pro' : (b.attributes.model_id ?? 'Bridge')} · {b.attributes.bridge_id}</span
									></td
								>
								<td class="hc-num">{b.attributes.host}</td>
								<td><span class="hc-dot {c.tone}"></span> {c.word}</td>
								<td class="hc-quiet hc-num"
									>{num(b.attributes.light_count) ?? '—'} lights · {num(b.attributes.sensor_count) ?? '—'} sensors · {num(b.attributes.scene_count) ?? '—'} scenes{b.attributes
										.update_state === 'ready_to_install'
										? ' · update waiting in the Hue app'
										: ''}</td
								>
								<td class="actions">
									<button class="hc-pill" onclick={() => hc.command(b.device_id, { action: 'refresh' })}>Refresh</button>
									{#if admin}<button class="hc-pill danger" onclick={() => remove(b)}>Remove</button>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<p class="hc-quiet">No bridge yet. Add one: homeCore looks for it on the network, and you press its button.</p>
		{/if}
	</section>

	<div class="cols">
		<section class="hc-card">
			<div class="hc-head"><h2>Rooms and zones</h2><span class="hc-quiet">as set up in the Hue app</span></div>
			<p class="hc-quiet">
				Each room or zone can be a device of its own in homeCore: one switch and one dimmer for all its lights, a card, and its
				scenes in automations. Changing it restarts the plugin.
			</p>
			<table class="hc-table">
				<thead><tr><th>Room or zone</th><th>Lights · scenes</th><th>As a device</th></tr></thead>
				<tbody>
					{#each rooms as r (r.id)}
						<tr>
							<td
								>{#if published(r)}<button class="link" onclick={() => hc.ui.openDevice(r.device_id)}>{r.name}</button>{:else}{r.name}{/if}
								{#if r.kind === 'zone'}<span class="hc-quiet">zone</span>{/if}
								{#if bridges.length > 1}<span class="hc-quiet">· {bridges.find((b) => b.attributes.bridge_id?.toLowerCase() === r.bridge_id?.toLowerCase())?.name ?? ''}</span>{/if}
							</td>
							<td class="hc-num">{r.lights} · {r.scenes}</td>
							<td>
								<button
									class="tog"
									class:off={!published(r)}
									role="switch"
									aria-checked={published(r)}
									aria-label={`${r.name} as a device`}
									disabled={!admin || saving !== ''}
									onclick={() => toggleRoom(r)}
								></button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</section>

		<div class="side">
			<section class="hc-card">
				<h2>Needs a look</h2>
				{#each attention as x (`${x.d.device_id}:${x.why}`)}
					<button class="warn" onclick={() => hc.ui.openDevice(x.d.device_id)}
						><span class="hc-dot bad"></span><span><b>{hc.devices.label(x.d)}</b> — {x.why}</span></button
					>
				{:else}
					<p class="hc-quiet">Nothing. Every light answers, and no battery is low.</p>
				{/each}
				<p class="hc-quiet">Settings › Alerts can send these to you.</p>
			</section>
			<section class="hc-card">
				<h2>Alerts it can raise</h2>
				<dl>
					<dt>Bridge not answering</dt><dd>error</dd>
					<dt>Light not answering</dt><dd>warning</dd>
					<dt>Software update waiting</dt><dd>information · off until wanted</dd>
				</dl>
				<p class="hc-quiet">A low battery is homeCore's own alert, for every device that has one.</p>
			</section>
		</div>
	</div>

	{#if admin && config}
		<section class="hc-card">
			<div class="hc-head">
				<h2>Settings</h2>
				<button class="link" onclick={() => hc.ui.navigate(`/settings/plugins/${encodeURIComponent(hc.pluginId)}?tab=settings`)}>All settings ›</button>
			</div>
			<div class="settings">
				<span class="hc-field"
					><span class="hc-label">Temperature in</span>
					<span class="hc-row">
						{#each [['c', '°C'], ['f', '°F']] as [v, label] (v)}
							<button
								class="hc-pill"
								class:go={(hue.display?.temperature_unit ?? 'c') === v}
								disabled={saving !== ''}
								onclick={() => setting({ display: { ...(hue.display ?? {}), temperature_unit: v } }, 'unit')}>{label}</button
							>
						{/each}
					</span></span
				>
				<span class="hc-field"
					><span class="hc-label">Entertainment areas</span>
					<span class="hc-row"
						><button
							class="tog"
							class:off={!hue.publish_entertainment_configurations}
							role="switch"
							aria-checked={!!hue.publish_entertainment_configurations}
							aria-label="Show entertainment areas"
							disabled={saving !== ''}
							onclick={() => setting({ publish_entertainment_configurations: !hue.publish_entertainment_configurations }, 'ent')}
						></button>{hue.publish_entertainment_configurations ? 'shown as devices' : 'not shown'}</span
					></span
				>
				<span class="hc-field"
					><span class="hc-label">Live updates</span>
					<span class="hc-row"
						><button
							class="tog"
							class:off={hue.eventstream_enabled === false}
							role="switch"
							aria-checked={hue.eventstream_enabled !== false}
							aria-label="Live updates from the bridge"
							disabled={saving !== ''}
							onclick={() => setting({ eventstream_enabled: hue.eventstream_enabled === false }, 'live')}
						></button>{hue.eventstream_enabled === false ? `checked every ${hue.resync_interval_secs ?? 60} s` : 'on'}</span
					></span
				>
			</div>
			{#if saving}<p class="hc-quiet">Saving, and restarting the plugin…</p>{/if}
		</section>
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 16px;
		container-type: inline-size;
	}
	h2 {
		margin: 0;
		font-size: 17px;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.scroll {
		overflow-x: auto;
	}
	.hc-table {
		width: 100%;
	}
	td {
		vertical-align: middle;
	}
	.actions {
		white-space: nowrap;
		text-align: right;
	}
	.actions .hc-pill {
		min-height: 32px;
	}
	.cols {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 16px;
	}
	@container (min-width: 820px) {
		.cols {
			grid-template-columns: minmax(0, 1.7fr) minmax(0, 1fr);
			align-items: start;
		}
	}
	.side {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
	}
	.warn {
		display: flex;
		gap: 10px;
		align-items: center;
		text-align: left;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--alert-bg);
		border: 1px solid var(--alert-line);
		font-size: 14px;
	}
	dl {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 6px 12px;
		margin: 0;
		font-size: 14px;
	}
	dd {
		margin: 0;
		color: var(--ink-muted);
	}
	.settings {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 16px;
	}
	.tog {
		width: 46px;
		height: 28px;
		border-radius: 14px;
		background: var(--track-on, var(--accent));
		position: relative;
		flex: none;
		border: none;
	}
	.tog::after {
		content: '';
		position: absolute;
		right: 3px;
		top: 3px;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: #fff;
	}
	.tog.off {
		background: var(--track-off, #cfc8ba);
	}
	.tog.off::after {
		right: auto;
		left: 3px;
	}
	.tog:disabled {
		opacity: 0.5;
	}
</style>
