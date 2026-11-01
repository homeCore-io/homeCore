<svelte:options customElement={{ tag: 'hc-weather-page', props: { host: { type: 'Object' } } }} />

<!-- hc-weather's page: each place, the weather there now, whose warnings it
     gets and any in force, when each reading was last fetched (or why it
     wasn't), and where the data comes from. Its settings are the app's
     generic form, below this page. -->
<script>
	/** @type {{ host: import('../../../../sdk/ui/index.js').Host }} */
	let { host: hc } = $props();
	const me = $host();

	// The app's parts (hc-card, hc-pill, …) in this shadow root.
	$effect(() => {
		const root = me?.shadowRoot;
		const sheet = hc?.theme?.sheet;
		if (root && sheet && !root.adoptedStyleSheets.includes(sheet))
			root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
	});

	// The person's units (app 0.1.21 and later); without them, homeCore's own.
	let chosen = $state(undefined);
	$effect(() => hc?.units?.subscribe((u) => (chosen = u)));
	function show(v, unit) {
		void chosen;
		if (typeof v !== 'number' || !Number.isFinite(v)) return '—';
		return hc?.units ? hc.units.show(v, unit).text : `${Math.round(v * 10) / 10}${unit === '°C' ? '°C' : ` ${unit}`}`;
	}

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	let alerts = $state([]);
	$effect(() => hc?.alerts?.subscribe((list) => (alerts = list ?? [])));

	let places = $state(null);
	let trouble = $state('');
	async function load() {
		const r = await hc.query('status', {});
		if (r.ok) {
			places = r.data.places ?? [];
			trouble = '';
		} else trouble = r.error;
	}
	$effect(() => {
		if (!hc) return;
		void load();
		const t = setInterval(load, 30_000);
		return () => clearInterval(t);
	});

	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 10_000);
		return () => clearInterval(t);
	});
	function ago(iso) {
		const t = Date.parse(iso ?? '');
		if (!Number.isFinite(t)) return 'not yet';
		const s = Math.max(0, Math.round((now - t) / 1000));
		if (s < 60) return 'just now';
		if (s < 3600) return `${Math.round(s / 60)} min ago`;
		return `${Math.round(s / 3600)} h ago`;
	}

	const SOURCES = [
		['now', 'The weather now'],
		['forecast', 'Forecast'],
		['air', 'Air quality'],
		['warnings', 'Warnings']
	];

	const deviceOf = (p) => devices.find((d) => d.device_id === p.device_id);
	// The labels the device's schema gives `condition` (conditions.rs).
	const SKY = {
		clear: 'Clear',
		partly_cloudy: 'Partly cloudy',
		cloudy: 'Cloudy',
		fog: 'Fog',
		drizzle: 'Drizzle',
		rain: 'Rain',
		freezing_rain: 'Freezing rain',
		snow: 'Snow',
		showers: 'Showers',
		thunderstorm: 'Thunderstorm'
	};
	const sky = (d) => {
		const v = d?.attributes?.condition;
		return typeof v === 'string' ? (SKY[v] ?? v.replace(/_/g, ' ')) : '';
	};
	const warningsAt = (p) => alerts.filter((a) => a.device_id === p.device_id);

	let busy = $state(false);
	async function refresh() {
		busy = true;
		await hc.action('refresh', {});
		setTimeout(() => {
			busy = false;
			void load();
		}, 3000);
	}
	const may = $derived(hc?.me?.can?.('devices:write') ?? hc?.me?.isAdmin ?? false);
</script>

<div class="page">
	{#if trouble}<p class="warn" role="status">{trouble}</p>{/if}
	{#if places === null}
		<p class="hc-quiet">Asking the plugin…</p>
	{:else if places.length === 0}
		<section class="hc-card">
			<h3>No places yet</h3>
			<p class="hc-quiet">
				With no places in its settings, the plugin reports the weather at the house, where homeCore's
				location says it is. If nothing shows here, homeCore didn't say: add a place below.
			</p>
		</section>
	{/if}

	{#each places ?? [] as p (p.id)}
		{@const d = deviceOf(p)}
		{@const a = d?.attributes ?? {}}
		{@const w = warningsAt(p)}
		<section class="hc-card">
			<div class="hc-head">
				<h3>
					<span class="hc-dot" class:good={Object.keys(p.failing ?? {}).length === 0} class:bad={Object.keys(p.failing ?? {}).length > 0}></span>
					{p.name}
				</h3>
				{#if d}<button class="hc-pill" onclick={() => hc.ui.openDevice(p.device_id)}>Details</button>{/if}
			</div>
			<div class="now">
				<span class="temp hc-num">{show(a.temperature, '°C')}</span>
				<span class="sky">{sky(d)}</span>
				<span class="hc-quiet">
					{#if typeof a.today_high === 'number'}High {show(a.today_high, '°C')} · low {show(a.today_low, '°C')}{/if}
				</span>
			</div>
			{#if w.length}
				<ul class="warnings">
					{#each w as x (x.id ?? x.reason)}<li class:bad={x.severity === 'error'}>{x.reason ?? x.message}</li>{/each}
				</ul>
			{/if}
			<div class="facts">
				<span><span class="hc-label">Where</span> <span class="hc-num">{p.latitude.toFixed(3)}, {p.longitude.toFixed(3)}</span></span>
				<span><span class="hc-label">Warnings from</span> {p.warnings_from}</span>
			</div>
			<table class="hc-table">
				<tbody>
					{#each SOURCES as [k, label] (k)}
						<tr>
							<td>{label}</td>
							<td class:late={p.failing?.[k]}>
								{#if p.failing?.[k]}Can't read it: {p.failing[k]}{:else}{ago(p.updated?.[k])}{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</section>
	{/each}

	{#if may && places?.length}
		<div class="hc-row">
			<button class="hc-pill" disabled={busy} onclick={refresh}>{busy ? 'Refreshing…' : 'Refresh now'}</button>
		</div>
	{/if}

	<p class="hc-quiet foot">
		Weather data by <a href="https://open-meteo.com/" target="_blank" rel="noreferrer">Open-Meteo.com</a>
		(<a href="https://creativecommons.org/licenses/by/4.0/" target="_blank" rel="noreferrer">CC BY 4.0</a>).
		Warnings from the US National Weather Service and
		<a href="https://meteoalarm.org/" target="_blank" rel="noreferrer">MeteoAlarm</a>. This page comes with
		the Weather plugin.
	</p>
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	h3 {
		margin: 0;
		font-size: 17px;
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.now {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 14px;
	}
	.temp {
		font-size: 32px;
		line-height: 1;
	}
	.sky {
		font-weight: 600;
	}
	.facts {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 22px;
		font-size: 14px;
	}
	.warnings {
		margin: 0;
		padding: 0 0 0 18px;
		font-size: 14px;
	}
	.warnings li.bad {
		color: var(--alert);
		font-weight: 600;
	}
	.warn,
	.late {
		color: var(--alert);
	}
	.foot {
		font-size: 12px;
	}
	.foot a {
		color: inherit;
	}
</style>
