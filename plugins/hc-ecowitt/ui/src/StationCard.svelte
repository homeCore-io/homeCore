<svelte:options
	customElement={{
		tag: 'hc-ecowitt-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A weather station's card: how it is outside, at a glance. Small: one
     line. Standard: the temperature, humidity, wind and rain. Large: the
     wind on a compass beside the rest. Tap for its panel. -->
<script>
	import Compass from './lib/Compass.svelte';
	import { DEG as deg, adopt, ago, compass, fmt, isRaining, num, rainGauge, stale } from './lib/weather.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const station = $derived(devices.find((d) => d.device_id === deviceId));
	const rain = $derived(rainGauge(devices));
	const s = $derived(station?.attributes ?? {});
	const r = $derived(rain?.attributes ?? {});
	const today = $derived(r.rain_daily_mm);
	const raining = $derived(isRaining(r));
	const name = $derived(station ? hc.devices.label(station) : 'Weather');
	const open = () => station && hc.ui.openDevice(station.device_id);
</script>

{#if station}
	<button class="hc-tile {form}" class:off={!station.available} onclick={open}>
		{#if form === 'compact'}
			<span class="row">
				<span class="hc-tile-name">{name}</span>
				<span class="hc-num big-c">{fmt(s.temperature)}{deg}</span>
			</span>
			<span class="hc-tile-state"
				>{fmt(s.humidity, 0)}% · wind {fmt(s.wind_speed)} m/s {compass(s.wind_direction)}{raining
					? ' · raining'
					: ''}</span
			>
		{:else}
			<span class="row top">
				<span class="hc-tile-name">{name}</span>
				<span class="hc-tile-state" class:late={stale(station.last_seen)}>{ago(station.last_seen)}</span>
			</span>
			<span class="body" class:wide={form === 'large'}>
				<span class="now">
					<span class="temp"><b class="hc-num">{fmt(s.temperature)}</b><i>{deg}</i></span>
					<span class="hc-tile-state">{fmt(s.humidity, 0)}% humidity</span>
				</span>
				{#if form === 'large'}
					<span class="dial"
						><Compass dir={s.wind_direction} avg={s.wind_direction_avg10m} speed={s.wind_speed} gust={s.wind_gust} /></span
					>
				{/if}
				<span class="facts">
					{#if form !== 'large'}
						<span
							><span class="arrow" style:transform={`rotate(${(num(s.wind_direction) ?? 0) + 180}deg)`}>↑</span>
							<span class="hc-num">{fmt(s.wind_speed)}</span> m/s {compass(s.wind_direction)}</span
						>
					{/if}
					<span class:wet={raining}
						>{raining ? 'Raining · ' : ''}<span class="hc-num">{fmt(today)}</span> mm today</span
					>
					{#if form === 'large'}
						<span>UV <span class="hc-num">{fmt(s.uvi, 0)}</span> · <span class="hc-num">{fmt(s.pressure, 0)}</span> hPa</span>
					{/if}
				</span>
			</span>
		{/if}
	</button>
{/if}

<style>
	button.hc-tile {
		width: 100%;
		font: inherit;
		text-align: left;
		cursor: pointer;
		container-type: inline-size;
	}
	.off {
		opacity: 0.6;
	}
	.row {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 8px;
		min-width: 0;
	}
	.big-c {
		font-size: 18px;
		font-weight: 600;
	}
	.body {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
		gap: 10px;
	}
	.body.wide {
		align-items: center;
	}
	.now {
		display: flex;
		flex-direction: column;
	}
	.temp {
		display: flex;
		align-items: flex-start;
		line-height: 1;
	}
	.temp b {
		font-size: 34px;
		font-weight: 600;
	}
	.temp i {
		font-style: normal;
		font-size: 14px;
		color: var(--ink-muted);
		margin-top: 3px;
	}
	.dial {
		width: 120px;
		flex: none;
	}
	.facts {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: 2px;
		font-size: 13px;
		color: var(--ink-muted);
		text-align: right;
	}
	.arrow {
		display: inline-block;
		color: var(--accent);
		font-weight: 700;
	}
	.wet {
		color: var(--accent);
		font-weight: 600;
	}
	.late {
		color: var(--alert);
	}
	/* A phone's half-width card: the time goes, the facts go under. */
	@container (max-width: 200px) {
		.top .hc-tile-state {
			display: none;
		}
		.body {
			flex-direction: column;
			align-items: flex-start;
			gap: 4px;
		}
		.facts {
			align-items: flex-start;
			text-align: left;
		}
	}
</style>
