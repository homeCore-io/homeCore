<!-- A device's own settings — its configuration parameters, named as its
     device file describes them. A change goes to the device straight away. -->
<script>
	import { failed } from './zwave.js';

	let { hc, deviceId, dev = null, admin = false, onloaded } = $props();
	const childLock = $derived(dev?.attributes?.child_lock);

	let params = $state(null);
	let error = $state('');
	let busy = $state('');
	let saved = $state('');

	async function load(refresh = false) {
		busy = refresh ? 'refresh' : busy;
		const r = await hc.query('get_config_params', { device_id: deviceId, refresh });
		busy = '';
		const err = failed(r);
		if (err) {
			error = err;
			params = params ?? [];
		} else {
			error = '';
			params = r.data.params ?? [];
		}
		onloaded?.(params.length);
	}
	$effect(() => {
		if (hc && deviceId) void load();
	});

	async function set(p, raw) {
		const value = Number(raw);
		if (!Number.isFinite(value)) return;
		busy = String(p.param);
		error = '';
		saved = '';
		const r = await hc.action('set_config_param', { device_id: deviceId, param: p.param, property_key: p.property_key ?? undefined, value });
		busy = '';
		const err = failed(r);
		if (err) error = `${p.label}: ${err}`;
		else saved = `${p.label} changed.`;
		await load();
	}

	async function defaults() {
		const ok = await hc.ui.confirm({
			title: 'Put every setting back to its default?',
			body: 'Each one the device has a default for goes back to it, on the device.',
			go: 'Back to defaults'
		});
		if (!ok) return;
		busy = 'defaults';
		const r = await hc.action('reset_config_params', { device_id: deviceId });
		busy = '';
		error = failed(r);
		if (!error) saved = 'Back to its defaults.';
		await load();
	}

	const help = (p) => p.description || (p.min !== null && p.max !== null && p.min !== undefined ? `${Number(p.min).toLocaleString()}–${Number(p.max).toLocaleString()}` : '');
	const isDefault = (p) => p.default === null || p.default === undefined || p.value === p.default;
</script>

<div class="settings">
	<span class="hc-quiet lead">The device’s own settings, named as it describes them. Changes go to the device straight away.</span>
	{#if childLock !== undefined}
		<div class="list">
			<div class="row">
				<span class="words"><span class="label">Its own buttons</span><span class="help">Child lock: the buttons on the device do nothing; homeCore still works it</span></span>
				<select
					aria-label="Its own buttons"
					value={childLock ? 'locked' : 'work'}
					disabled={!admin || busy === 'child'}
					onchange={async (e) => {
						busy = 'child';
						error = '';
						const r = await hc.command(deviceId, { child_lock: e.currentTarget.value === 'locked' });
						busy = '';
						if (!r.ok) error = r.error;
					}}
				>
					<option value="work">Work</option>
					<option value="locked">Do nothing</option>
				</select>
			</div>
		</div>
	{/if}
	{#if params === null}
		<p class="hc-quiet">Reading its settings…</p>
	{:else if !params.length}
		<p class="hc-quiet">It has no settings of its own.</p>
	{:else}
		<div class="list">
			{#each params as p (`${p.param}/${p.property_key ?? ''}`)}
				<div class="row">
					<span class="words"><span class="label">{p.label}</span>{#if help(p)}<span class="help">{help(p)}</span>{/if}</span>
					{#if p.states?.length && !p.allow_manual_entry}
						<select
							aria-label={p.label}
							value={String(p.value)}
							disabled={!admin || !p.writeable || busy === String(p.param)}
							onchange={(e) => set(p, e.currentTarget.value)}
						>
							{#each p.states as s (s.value)}<option value={String(s.value)}>{s.label}</option>{/each}
						</select>
					{:else}
						<span class="numin"
							><input
								type="number"
								aria-label={p.label}
								value={p.value}
								min={p.min ?? undefined}
								max={p.max ?? undefined}
								disabled={!admin || !p.writeable || busy === String(p.param)}
								onchange={(e) => set(p, e.currentTarget.value)}
							/>{#if p.unit}<span class="unit">{p.unit}</span>{/if}</span
						>
					{/if}
					{#if !isDefault(p)}<span class="tag">not the default</span>{/if}
				</div>
			{/each}
		</div>
	{/if}
	{#if error}<span class="err" role="alert">{error}</span>{/if}
	{#if saved}<span class="ok">{saved}</span>{/if}
	<div class="hc-row">
		<button class="hc-pill" disabled={!!busy} onclick={() => load(true)}>{busy === 'refresh' ? 'Reading…' : 'Read them from the device'}</button>
		{#if admin && params?.length}<button class="hc-pill" disabled={!!busy} onclick={defaults}>Back to defaults</button>{/if}
	</div>
	{#if !admin}<span class="hc-quiet">Only admins can change them.</span>{/if}
</div>

<style>
	.settings {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.lead {
		font-size: 14px;
	}
	.list {
		display: flex;
		flex-direction: column;
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: 16px;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 12px;
		min-height: 64px;
		padding: 8px 16px;
		border-top: 1px solid var(--card-line);
	}
	.row:first-child {
		border-top: none;
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 2px;
		flex: 1 1 200px;
		min-width: 0;
	}
	.label {
		font-weight: 600;
		font-size: 15px;
	}
	.help {
		font-size: 12px;
		color: var(--ink-muted);
	}
	select,
	input {
		min-height: 40px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 10px;
	}
	select {
		min-width: 170px;
		max-width: 100%;
	}
	.numin {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.numin input {
		width: 96px;
	}
	.unit {
		font-size: 13px;
		color: var(--ink-muted);
	}
	.tag {
		font-size: 12px;
		color: var(--lit-ink, var(--accent));
		background: color-mix(in srgb, var(--lit-bg) 60%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--accent) 40%, var(--card-line));
		padding: 3px 7px;
		border-radius: 8px;
	}
	.hc-pill {
		min-height: 40px;
	}
	.err {
		color: var(--alert);
	}
	.ok {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
</style>
