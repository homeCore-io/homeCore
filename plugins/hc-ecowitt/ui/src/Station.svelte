<svelte:options
	customElement={{
		tag: 'hc-ecowitt-station',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A weather station's panel: the weather now, its day, and the other
     sensors on the same gateway. -->
<script>
	import Now from './lib/Now.svelte';
	import Spark from './lib/Spark.svelte';
	import { adopt, ago, kindOf, reading, trend } from './lib/weather.js';

	/** @type {{ host: import('../../../../sdk/ui/index.js').Host, deviceId?: string }} */
	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));

	const DAY = 24 * 3600_000;
	let temp = $state([]);
	let wind = $state([]);
	let pressure = $state([]);
	$effect(() => {
		if (!hc || !deviceId) return;
		const from = new Date(Date.now() - DAY).toISOString();
		const get = (attr) =>
			hc.devices.history(deviceId, attr, { from, limit: 3000 }).then((r) => (r.ok ? r.data : []));
		void get('temperature').then((x) => (temp = x));
		void get('wind_speed').then((x) => (wind = x));
		void get('barometric_rel').then((x) => (pressure = x));
	});
	const pressureTrend = $derived(trend(pressure.filter((p) => Date.parse(p.at) > Date.now() - 3 * 3600_000)));
	const others = $derived(
		devices.filter((d) => d.device_id !== deviceId && d.device_type !== 'gateway' && d.device_type !== 'weather_station')
	);
</script>

<div class="station">
	<Now {devices} {pressureTrend} />

	<section class="hc-card">
		<span class="hc-label">The last day</span>
		<div class="days">
			<Spark points={temp} label="Temperature" unit="°" />
			<Spark points={wind} label="Wind" unit=" mph" />
			<Spark points={pressure} label="Pressure" unit=" inHg" digits={2} />
		</div>
	</section>

	{#if others.length}
		<section class="hc-card">
			<span class="hc-label">On the same gateway</span>
			<ul>
				{#each others as d (d.device_id)}
					<li>
						<button type="button" onclick={() => hc.ui.openDevice(d.device_id)}>
							<span class="name">{hc.devices.label(d)}</span>
							<span class="hc-quiet">{kindOf(d.device_type)}{d.room ? ` · ${d.room}` : ''}</span>
							<span class="hc-num what">{reading(d)}</span>
							<span class="hc-quiet">{ago(d.last_seen)}</span>
						</button>
					</li>
				{/each}
			</ul>
		</section>
	{/if}
</div>

<style>
	.station {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.days {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 200px), 1fr));
		gap: 14px;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	li + li {
		border-top: 1px solid var(--card-line);
	}
	button {
		width: 100%;
		display: grid;
		grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr) minmax(0, 1.4fr) auto;
		gap: 8px;
		align-items: baseline;
		padding: 10px 0;
		border: none;
		background: none;
		color: var(--ink);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.name {
		font-weight: 600;
	}
	@media (max-width: 560px) {
		button {
			grid-template-columns: 1fr auto;
		}
		.what {
			grid-column: 1 / -1;
		}
	}
</style>
