<svelte:options customElement={{ tag: 'hc-weather-page', props: { host: { type: 'Object' } } }} />

<!-- hc-weather's page: each place, the weather there now and coming up,
     the next 24 hours, whose warnings it gets and any in force in full, when each reading was last fetched (or why it
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

	// A temperature in whole degrees, as the app's Weather card shows it: "56°F".
	function deg(v) {
		void chosen;
		if (typeof v !== "number" || !Number.isFinite(v)) return '—';
		const r = hc?.units ? hc.units.show(v, '°C') : { value: v, unit: '°C' };
		return `${Math.round(r.value)}${r.unit}`;
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

	// "3 PM", "Sun 3 PM": a time in the viewer's clock, with the day when it isn't today.
	function when(iso) {
		const t = new Date(iso ?? '');
		if (!Number.isFinite(t.getTime())) return '';
		const today = new Date(now).toDateString() === t.toDateString();
		return t.toLocaleString(undefined, {
			weekday: today ? undefined : 'short',
			hour: 'numeric',
			minute: t.getMinutes() ? '2-digit' : undefined
		});
	}
	const LEVEL = ['', 'Advisory', 'Watch', 'Warning'];

	// What's coming: rain soon, tonight's low, tomorrow.
	function coming(a) {
		const out = [];
		if (typeof a.rain_starts_in_min === 'number')
			out.push(a.rain_starts_in_min === 0 ? 'Rain starting now' : `Rain in about ${a.rain_starts_in_min} min`);
		if (typeof a.tonight_low === 'number')
			out.push(`${a.freeze_tonight ? 'Freezing tonight' : 'Tonight'}, low ${deg(a.tonight_low)}`);
		if (typeof a.tomorrow_high === 'number')
			out.push(`Tomorrow ${deg(a.tomorrow_high)} / ${deg(a.tomorrow_low)}`);
		if (typeof a.snow_today_mm === 'number' && a.snow_today_mm > 0) out.push(`Snow today ${show(a.snow_today_mm, 'mm')}`);
		return out;
	}

	// Every third hour of the next 24.
	const hoursOf = (a) => (Array.isArray(a.forecast_hourly) ? a.forecast_hourly.filter((_, i) => i % 3 === 0) : []);

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
				<span class="temp hc-num">{deg(a.temperature)}</span>
				<span class="sky">{sky(d)}</span>
				<span class="hc-quiet">
					{#if typeof a.today_high === 'number'}High {deg(a.today_high)} · low {deg(a.today_low)}{/if}
				</span>
			</div>
			{#if coming(a).length}
				<p class="coming">{coming(a).join(' · ')}</p>
			{/if}
			{#if p.warnings?.length}
				<div class="warnings">
					{#each p.warnings as x (x.id)}
						<details class:bad={x.level === 3}>
							<summary>
								<span class="lvl">{LEVEL[x.level] ?? 'Notice'}</span>
								{x.event}{#if x.until}<span class="hc-quiet">{' · until '}{when(x.until)}</span>{/if}
							</summary>
							{#if x.since && Date.parse(x.since) > now}<p class="hc-quiet">From {when(x.since)}</p>{/if}
							{#if x.description}<p class="text">{x.description}</p>{/if}
							{#if x.instruction}<p class="text do">{x.instruction}</p>{/if}
						</details>
					{/each}
				</div>
			{:else if w.length}
				<ul class="warnings">
					{#each w as x (x.id ?? x.reason)}<li class:bad={x.severity === 'error'}>{x.reason ?? x.message}</li>{/each}
				</ul>
			{/if}
			{#if hoursOf(a).length}
				<div class="hours" role="list" aria-label="The next 24 hours">
					{#each hoursOf(a) as h (h.time)}
						<div class="hour" role="listitem">
							<span class="hc-quiet">{when(h.time)}</span>
							<span class="hc-num">{deg(h.temperature)}</span>
							<span class="rain" class:none={!h.precipitation_probability}>{h.precipitation_probability ?? 0}%</span>
						</div>
					{/each}
				</div>
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
	.warnings li.bad,
	details.bad summary {
		color: var(--alert);
		font-weight: 600;
	}
	div.warnings {
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	summary {
		cursor: pointer;
	}
	.lvl {
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		margin-right: 6px;
	}
	.text {
		white-space: pre-line;
		margin: 6px 0 0;
		font-size: 13px;
	}
	.do {
		font-weight: 600;
	}
	.coming {
		margin: 0;
		font-size: 14px;
	}
	.hours {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(64px, 1fr));
		gap: 8px;
		font-size: 13px;
	}
	.hour {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 2px;
	}
	.rain {
		color: var(--accent, inherit);
		font-size: 12px;
	}
	.rain.none {
		opacity: 0.45;
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
