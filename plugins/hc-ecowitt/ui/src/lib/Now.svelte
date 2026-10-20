<!-- The weather now, from the station and the sensors beside it: outside,
     wind, rain, the sky, and inside. -->
<script>
	import Compass from './Compass.svelte';
	import { DEG as deg, ago, fmt, isRaining, num, rainGauge, stale, uvWords } from './weather.js';

	/** @type {{ devices: import('./weather.js').Device[], pressureTrend?: string }} */
	let { devices = [], pressureTrend = '' } = $props();

	const of = (type) => devices.find((d) => d.device_type === type);
	const station = $derived(of('weather_station'));
	const rain = $derived(rainGauge(devices));
	const lightning = $derived(of('lightning_sensor'));
	const indoor = $derived(devices.find((d) => d.device_id.endsWith('_indoor')));
	const s = $derived(station?.attributes ?? {});
	const r = $derived(rain?.attributes ?? {});
	const raining = $derived(isRaining(r));
	const strikeAt = $derived(Number(lightning?.attributes.last_strike) * 1000);
</script>

{#if !station}
	<p class="hc-quiet">No weather station has reported yet.</p>
{:else}
	<div class="now">
		<section class="hc-card big">
			<div class="hc-head">
				<span class="hc-label">Outside</span>
				<span class="hc-quiet" class:late={stale(station.last_seen)}>{ago(station.last_seen)}</span>
			</div>
			<div class="temp"><b class="hc-num">{fmt(s.temperature)}</b><span>{deg}</span></div>
			<div class="facts">
				<span><span class="hc-label">Humidity</span> <b class="hc-num">{fmt(s.humidity, 0)}%</b></span>
				{#if num(s.feels_like) !== undefined}<span
						><span class="hc-label">Feels like</span> <b class="hc-num">{fmt(s.feels_like)}{deg}</b></span
					>{/if}
				{#if num(s.dewpoint) !== undefined}<span
						><span class="hc-label">Dew point</span> <b class="hc-num">{fmt(s.dewpoint)}{deg}</b></span
					>{/if}
				{#if num(s.vpd) !== undefined}<span
						><span class="hc-label">VPD</span> <b class="hc-num">{fmt(s.vpd, 2)} kPa</b></span
					>{/if}
			</div>
		</section>

		<section class="hc-card wind">
			<span class="hc-label">Wind</span>
			<Compass dir={s.wind_direction} avg={s.wind_direction_avg10m} speed={s.wind_speed} gust={s.wind_gust} />
			<span class="hc-quiet">Strongest today {fmt(s.wind_gust_daily_max)} m/s</span>
		</section>

		<section class="hc-card">
			<div class="hc-head">
				<span class="hc-label">Rain</span>
				{#if rain}<span class="hc-tag" class:good={!raining} class:warn={raining}
						>{raining ? 'Raining' : 'Dry'}</span
					>{/if}
			</div>
			{#if rain}
				<div class="temp small"><b class="hc-num">{fmt(r.rain_daily_mm)}</b><span>mm today</span></div>
				<dl>
					<dt>Now</dt><dd>{fmt(r.rain_rate_mm_hr)} mm/h</dd>
					<dt>This storm</dt><dd>{fmt(r.rain_event_mm)} mm</dd>
					<dt>Week</dt><dd>{fmt(r.rain_weekly_mm)} mm</dd>
					<dt>Month</dt><dd>{fmt(r.rain_monthly_mm)} mm</dd>
					<dt>Year</dt><dd>{fmt(r.rain_yearly_mm)} mm</dd>
				</dl>
			{:else}
				<p class="hc-quiet">No rain gauge.</p>
			{/if}
		</section>

		<section class="hc-card">
			<span class="hc-label">Sky and air</span>
			<dl>
				<dt>Pressure</dt>
				<dd>{fmt(s.pressure, 0)} hPa{pressureTrend ? ` · ${pressureTrend}` : ''}</dd>
				<dt>UV index</dt><dd>{fmt(s.uvi, 0)} {uvWords(s.uvi)}</dd>
				<dt>Sunlight</dt><dd>{fmt(s.solar_radiation, 0)} W/m²</dd>
				{#if lightning}
					<dt>Lightning</dt>
					<dd>
						{fmt(lightning.attributes.strike_count, 0)} strikes{Number.isFinite(strikeAt)
							? ` · last ${ago(new Date(strikeAt).toISOString())}, ${fmt(lightning.attributes.distance_km, 0)} km`
							: ''}
					</dd>
				{/if}
				{#if indoor}
					<dt>Inside</dt>
					<dd>{fmt(indoor.attributes.temperature)}{deg} · {fmt(indoor.attributes.humidity, 0)}%</dd>
				{/if}
			</dl>
		</section>
	</div>
{/if}

<style>
	.now {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
		gap: 14px;
	}
	.temp {
		display: flex;
		align-items: flex-start;
		gap: 4px;
		line-height: 1;
	}
	.temp b {
		font-size: 56px;
		font-weight: 600;
	}
	.temp span {
		font-size: 20px;
		color: var(--ink-muted);
		margin-top: 6px;
	}
	.temp.small b {
		font-size: 34px;
	}
	.temp.small span {
		font-size: 14px;
		align-self: flex-end;
		margin: 0 0 4px;
	}
	.facts {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 16px;
	}
	.wind {
		align-items: center;
	}
	.wind .hc-label {
		align-self: flex-start;
	}
	dl {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 4px 12px;
		margin: 0;
		font-size: 14px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
		font-variant-numeric: tabular-nums;
	}
	.late {
		color: var(--alert);
	}
</style>
