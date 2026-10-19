<svelte:options
	customElement={{
		tag: 'hc-ecowitt-weather',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Weather now", for a page: the station's weather at a glance — large,
     with UV, lightning and the sensors that need a battery. "Details" opens
     the station's panel. -->
<script>
	import { adopt, compass, fmt, num, trend, uvWords } from './lib/weather.js';

	let { host: hc, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const station = $derived(devices.find((d) => d.device_type === 'weather_station'));
	const sensors = $derived(devices.filter((d) => d.device_type !== 'gateway'));
	const low = $derived(sensors.filter((d) => d.attributes.battery_low === true));
	const s = $derived(station?.attributes ?? {});
	const rain = $derived(
		devices.find((d) => d.device_type === 'rain_sensor' && d.attributes.rain_day_piezo !== undefined) ??
			devices.find((d) => d.device_type === 'rain_sensor')
	);
	const r = $derived(rain?.attributes ?? {});
	const indoor = $derived(devices.find((d) => d.device_id.endsWith('_indoor')));
	const lightning = $derived(devices.find((d) => d.device_type === 'lightning_sensor'));
	const deg = $derived(s.temperature_unit ?? '°F');
	// Which way the pressure is going: the last three hours of it.
	let pressure = $state([]);
	$effect(() => {
		if (!hc || !station) return;
		const from = new Date(Date.now() - 3 * 3600_000).toISOString();
		void hc.devices
			.history(station.device_id, 'barometric_rel', { from, limit: 500 })
			.then((x) => (pressure = x.ok ? x.data : []));
	});
</script>

<div class="hc-tile weather">
	<div class="head">
		<span class="hc-tile-name">Weather now</span>
		{#if station}<button class="link" onclick={() => hc.ui.openDevice(station.device_id)}>Details</button>{/if}
	</div>
	{#if station}
		<div class="glance" class:narrow={form !== 'large'}>
			<span class="temp"><b class="hc-num">{fmt(s.temperature)}</b><i>{deg}</i></span>
			<dl>
				<dt>Humidity</dt><dd>{fmt(s.humidity, 0)}%</dd>
				<dt>Wind</dt>
				<dd>
					<span class="arrow" style:transform={`rotate(${(num(s.wind_direction) ?? 0) + 180}deg)`}>↑</span>
					{fmt(s.wind_speed)} mph {compass(s.wind_direction)}
				</dd>
				<dt>Rain</dt><dd>{fmt(r.rain_day_piezo ?? r.rain_day, 2)} in today</dd>
				<dt>Pressure</dt><dd>{fmt(s.barometric_rel, 2)} inHg{trend(pressure) ? ` · ${trend(pressure)}` : ''}</dd>
				{#if indoor}<dt>Inside</dt><dd>{fmt(indoor.attributes.temperature)}{deg}</dd>{/if}
				{#if form === 'large'}
					<dt>UV</dt><dd>{fmt(s.uvi, 0)} {uvWords(s.uvi)}</dd>
					{#if lightning}<dt>Lightning</dt><dd>{fmt(lightning.attributes.strike_count, 0)} strikes today, nearest {fmt(lightning.attributes.distance, 0)} mi</dd>{/if}
				{/if}
			</dl>
		</div>
		{#if form === 'large' && low.length}
			<p class="hc-tile-state warn">Low battery: {low.map((d) => hc.devices.label(d)).join(', ')}</p>
		{/if}
	{:else}
		<p class="hc-tile-state">No weather station has reported yet.</p>
	{/if}
</div>

<style>
	.head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}
	.link {
		font: inherit;
		font-size: 13px;
		border: none;
		background: none;
		color: var(--accent);
		cursor: pointer;
		padding: 0;
	}
	.warn {
		color: var(--alert);
	}
	.glance {
		display: flex;
		align-items: center;
		gap: 14px;
	}
	/* One column wide: the temperature over the rest. */
	.glance.narrow {
		flex-direction: column;
		align-items: stretch;
		gap: 6px;
	}
	.temp {
		display: flex;
		align-items: flex-start;
		line-height: 1;
	}
	.temp b {
		font-size: 40px;
		font-weight: 600;
	}
	.temp i {
		font-style: normal;
		font-size: 15px;
		color: var(--ink-muted);
		margin-top: 4px;
	}
	dl {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 2px 10px;
		margin: 0;
		font-size: 13px;
	}
	dt {
		color: var(--ink-muted);
	}
	dd {
		margin: 0;
		font-variant-numeric: tabular-nums;
	}
	.arrow {
		display: inline-block;
		color: var(--accent);
		font-weight: 700;
	}
</style>
