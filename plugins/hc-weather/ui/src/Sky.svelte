<svelte:options
	customElement={{
		tag: 'hc-weather-sky',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Weather, animated", for a page: the weather now as a moving scene. The
     sky picks the backdrop and day or night the sun or moon; the readings set
     how much happens (more cloud cover, more clouds; heavier rain, more and
     faster drops; wind slants them and speeds the clouds). Over it: the
     temperature, a warning ribbon, "rain in 20 min", the next hours and, when
     large, five days.

     CSS and SVG only. It stops moving when off screen or when the tab is
     hidden, and stands still for anyone who asks for reduced motion. -->
<script>
	import { ICONS, LABELS, SCENES, sceneOf, scatter } from './lib/sky.js';

	let { host: hc, form = 'standard', config = {} } = $props();
	const me = $host();

	$effect(() => {
		const root = me?.shadowRoot;
		const sheet = hc?.theme?.sheet;
		if (root && sheet && !root.adoptedStyleSheets.includes(sheet))
			root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
	});

	let chosen = $state(undefined);
	$effect(() => hc?.units?.subscribe((u) => (chosen = u)));
	const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined);
	/** A reading as the person reads it, whole: "56°", "13 mph". */
	function shown(v, unit) {
		void chosen;
		const x = num(v);
		if (x === undefined) return undefined;
		const r = hc?.units ? hc.units.show(x, unit) : { value: x, unit };
		return { value: Math.round(r.value), unit: r.unit };
	}
	const deg = (v) => {
		const s = shown(v, '°C');
		return s ? `${s.value}°` : '—';
	};

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list ?? [])));
	const places = $derived(devices.filter((d) => d.device_type === 'weather'));
	const device = $derived(places.find((d) => d.device_id === config?.device_id) ?? places[0]);
	const a = $derived(device?.attributes ?? {});

	const key = $derived(sceneOf(a.condition));
	const scene = $derived(SCENES[key]);
	const night = $derived(a.is_day === false);
	// Heavier than 1 mm in 15 minutes is heavy rain (4 mm an hour and up).
	const heavy = $derived((num(a.precipitation_mm) ?? 0) >= 1 || a.condition === 'thunderstorm');
	const wind = $derived(num(a.wind_speed) ?? 0);
	const cover = $derived(num(a.cloud_cover_pct));
	const parts = $derived(scatter(key, { heavy, wind, cover, light: a.condition === 'drizzle' }));

	const label = $derived(LABELS[a.condition] ?? scene.label);
	const COMPASS = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'];
	const windText = $derived.by(() => {
		const s = shown(a.wind_speed, 'm/s');
		if (!s) return '';
		const d = num(a.wind_direction);
		return `${s.value} ${s.unit}${d === undefined ? '' : ' ' + COMPASS[Math.round(d / 45) % 8]}`;
	});
	const today = $derived.by(() => {
		const snow = shown(a.snow_today_mm, 'mm');
		if (snow && snow.value > 0) return `${snow.value} ${snow.unit} of snow today`;
		const chance = num(a.rain_chance_pct);
		return chance === undefined ? '' : `Rain ${Math.round(chance)}% today`;
	});

	const warning = $derived((num(a.alert_level) ?? 0) >= 2 && typeof a.alert_headline === 'string' ? a.alert_headline : '');
	const severe = $derived((num(a.alert_level) ?? 0) >= 3);
	const wet = $derived(key === 'rain' || key === 'storm' || key === 'snow');
	const rainSoon = $derived.by(() => {
		const m = num(a.rain_starts_in_min);
		if (m === undefined || wet || m > 60) return '';
		return m === 0 ? 'Rain starting' : `Rain in ${m} min`;
	});

	// The next hours, every third one, as many as fit.
	let width = $state(360);
	$effect(() => {
		const ro = new ResizeObserver(([e]) => (width = e.contentRect.width));
		ro.observe(me);
		return () => ro.disconnect();
	});
	const iconFor = (condition, isNight) => {
		const k = sceneOf(condition);
		if (isNight && (k === 'clear' || k === 'partly')) return ICONS.moon;
		return ICONS[SCENES[k].icon];
	};
	const hourLabel = (iso) => new Date(iso).toLocaleTimeString(undefined, { hour: 'numeric' }).replace(':00', '');
	const hours = $derived.by(() => {
		const all = Array.isArray(a.forecast_hourly) ? a.forecast_hourly : [];
		const fit = Math.max(3, Math.min(6, Math.floor((width - 24) / 62)));
		const now = { t: 'Now', icon: iconFor(a.condition, night), temp: deg(a.temperature) };
		const later = all
			.filter((_, i) => i > 0 && i % 3 === 0)
			.map((h) => {
				const hr = new Date(h.time).getHours();
				return { t: hourLabel(h.time), icon: iconFor(h.condition, hr < 6 || hr >= 19), temp: deg(h.temperature) };
			});
		return [now, ...later].slice(0, fit);
	});
	const days = $derived.by(() => {
		const all = (Array.isArray(a.forecast) ? a.forecast : []).slice(0, 5);
		const lows = all.map((d) => num(shown(d.low, '°C')?.value)).filter((x) => x !== undefined);
		const highs = all.map((d) => num(shown(d.high, '°C')?.value)).filter((x) => x !== undefined);
		const lo = Math.min(...lows), hi = Math.max(...highs), span = Math.max(1, hi - lo);
		return all.map((d, i) => {
			const l = shown(d.low, '°C')?.value, h = shown(d.high, '°C')?.value;
			return {
				key: d.time,
				day: i === 0 ? 'Today' : new Date(d.time).toLocaleDateString(undefined, { weekday: 'short' }),
				icon: iconFor(d.condition, false),
				label: LABELS[d.condition] ?? '',
				lo: l ?? '—',
				hi: h ?? '—',
				from: l === undefined ? 0 : Math.round(((l - lo) / span) * 100),
				to: h === undefined ? 0 : Math.round(((hi - h) / span) * 100)
			};
		});
	});

	// Still when it can't be seen: off screen, or the tab hidden.
	let onScreen = $state(true);
	let tabShown = $state(typeof document === 'undefined' ? true : !document.hidden);
	$effect(() => {
		const io = new IntersectionObserver(([e]) => (onScreen = e.isIntersecting));
		io.observe(me);
		const vis = () => (tabShown = !document.hidden);
		document.addEventListener('visibilitychange', vis);
		return () => {
			io.disconnect();
			document.removeEventListener('visibilitychange', vis);
		};
	});
	const still = $derived(!onScreen || !tabShown);
	const name = $derived(config?.name || (device ? (hc?.devices?.label?.(device) ?? device.name ?? '').replace(/ weather$/i, '') : ''));
	const glass = $derived(scene.lightGlass && !night ? 'rgba(30,45,60,.18)' : 'rgba(255,255,255,.16)');
</script>

{#if !device}
	<div class="empty hc-tile">
		<span class="hc-tile-name">Weather</span>
		<p class="hc-tile-state">No place has reported its weather yet.</p>
	</div>
{:else}
	<div
		class="card {form}"
		class:still
		style:background={night ? scene.night : scene.day}
		role="group"
		aria-label={`${name}: ${deg(a.temperature)}, ${label}`}
	>
		<div class="scene" aria-hidden="true">
			{#if (key === 'clear' || key === 'partly') && !night}
				<div class="sun">
					<svg class="rays" viewBox="0 0 100 100"
						><path
							d="M50 4v12M50 84v12M4 50h12M84 50h12M17.5 17.5l8.5 8.5M74 74l8.5 8.5M17.5 82.5l8.5-8.5M74 26l8.5-8.5M30 7.5l3 8M67 84.5l3 8M7.5 70l8-3M84.5 33l8-3M7.5 30l8 3M84.5 67l8 3M30 92.5l3-8M67 15.5l3-8"
						/></svg
					>
					<span class="disc"></span>
				</div>
			{/if}
			{#if (key === 'clear' || key === 'partly') && night}
				{#each parts.stars as s, i (i)}
					<span class="star" style:left="{s.x}%" style:top="{s.y}%" style:width="{s.z}px" style:height="{s.z}px" style:animation-delay="{s.d}s"></span>
				{/each}
				<span class="moon"></span>
			{/if}
			{#each parts.clouds as c, i (i)}
				<div class="lane" style:top="{c.top}%" style:animation-duration="{c.dur}s" style:animation-delay="{c.del}s" style:opacity={c.op}>
					<svg width={c.w * (form === 'large' ? 1.2 : form === 'compact' ? 0.7 : 1)} viewBox="0 0 120 64"
						><g fill={night ? scene.cloudNight[i % 2] : scene.cloudDay[i % 2]}
							><circle cx="34" cy="40" r="18" /><circle cx="60" cy="28" r="25" /><circle cx="88" cy="40" r="17" /><rect
								x="30"
								y="36"
								width="62"
								height="22"
								rx="11"
							/></g
						></svg
					>
				</div>
			{/each}
			{#if key === 'storm'}
				<div class="flash"></div>
				<svg class="bolt" viewBox="0 0 24 40"><path d="M14 0L3 22h8L7 40l15-26h-9L19 0z" /></svg>
			{/if}
			{#if parts.drops.length}
				<div class="fall" style:transform="rotate({parts.slant}deg)">
					{#each parts.drops as p, i (i)}
						<span
							class="drop"
							style:left="{p.x}%"
							style:animation-duration="{p.dur}s"
							style:animation-delay="{p.del}s"
							style:opacity={p.op}
							style:background="linear-gradient(to bottom, transparent, rgba(225,238,255,.85) {p.len}%, transparent {p.len + 0.2}%)"
						></span>
					{/each}
				</div>
			{/if}
			{#if parts.flakes.length}
				<div class="fall" style:transform="rotate({parts.slant}deg)">
					{#each parts.flakes as f, i (i)}
						<span class="flake" style:left="{f.x}%" style:--s="{f.s}px" style:animation-duration="{f.dur}s" style:animation-delay="{f.del}s" style:opacity={f.op}></span>
					{/each}
				</div>
			{/if}
			{#each parts.fog as g, i (i)}
				<div class="mist" style:top="{g.top}%" style:height="{g.h}%" style:animation-duration="{g.dur}s" style:animation-delay="{g.del}s"></div>
			{/each}
		</div>

		<div class="body">
			{#if warning}
				<span class="ribbon" class:watch={!severe} title={warning}
					><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0zM12 9v4M12 17h.01" /></svg
					><span>{form === 'compact' ? (severe ? 'Warning' : 'Watch') : warning}</span></span
				>
			{/if}
			<div class="top">
				<div class="main">
					<button class="place" onclick={() => hc.ui.openDevice(device.device_id)}>{name}</button>
					<span class="temp hc-num">{deg(a.temperature)}</span>
					<span class="sky">{label}</span>
					<span class="range"
						>H {deg(a.today_high)} · L {deg(a.today_low)}{#if form !== 'compact' && num(a.feels_like) !== undefined}{' · Feels '}{deg(a.feels_like)}{/if}</span
					>
				</div>
				{#if form !== 'compact'}
					<div class="side">
						{#if rainSoon}<span class="chip glass" style:background={glass}
								><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 2.7l5.7 5.7a8 8 0 1 1-11.4 0z" /></svg>{rainSoon}</span
							>{/if}
						{#if windText}<span>Wind {windText}</span>{/if}
						{#if num(a.humidity) !== undefined}<span>Humidity {Math.round(a.humidity)}%</span>{/if}
						{#if form === 'large' && today}<span>{today}</span>{/if}
					</div>
				{/if}
			</div>
			{#if form === 'compact' && rainSoon}
				<span class="chip glass small" style:background={glass}>{rainSoon}</span>
			{/if}
			{#if form !== 'compact' && hours.length}
				<div class="hours glass" style:background={glass} style:grid-template-columns="repeat({hours.length}, 1fr)">
					{#each hours as h, i (i)}
						<span class="hour"
							><span class="t">{h.t}</span><svg viewBox="0 0 24 24" aria-hidden="true"><path d={h.icon} /></svg><span class="hc-num"
								>{h.temp}</span
							></span
						>
					{/each}
				</div>
			{/if}
			{#if form === 'large' && days.length}
				<div class="days glass" style:background={glass}>
					{#each days as d (d.key)}
						<span class="day"
							><b>{d.day}</b><svg viewBox="0 0 24 24" aria-hidden="true"><path d={d.icon} /></svg><span class="what">{d.label}</span><span
								class="lo hc-num">{d.lo}°</span
							><span class="bar"><span style:left="{d.from}%" style:right="{d.to}%"></span></span><span class="hi hc-num"
								>{d.hi}°</span
							></span
						>
					{/each}
				</div>
			{/if}
		</div>
	</div>
{/if}

<style>
	:host {
		display: block;
	}
	.empty {
		min-height: 120px;
	}
	.card {
		position: relative;
		overflow: hidden;
		isolation: isolate;
		border-radius: var(--radius, 18px);
		color: #fff;
		box-shadow:
			0 1px 2px rgba(0, 0, 0, 0.08),
			0 8px 24px rgba(20, 30, 50, 0.12);
		transition: background 1.2s ease;
	}
	.card.compact {
		min-height: 150px;
	}
	.card.standard {
		min-height: 236px;
	}
	.card.large {
		min-height: 420px;
	}
	.scene {
		position: absolute;
		inset: 0;
		overflow: hidden;
		z-index: 0;
		pointer-events: none;
	}
	/* A shade on the text side, so white words stay readable when a white
	   cloud drifts behind them. */
	.scene::after {
		content: '';
		position: absolute;
		inset: 0;
		background: linear-gradient(100deg, rgba(10, 20, 35, 0.3) 0%, rgba(10, 20, 35, 0.14) 38%, transparent 66%);
	}
	.body {
		position: relative;
		z-index: 1;
		box-sizing: border-box;
		min-height: inherit;
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px;
	}
	.compact .body {
		padding: 14px;
		gap: 8px;
	}
	.large .body {
		padding: 20px;
		gap: 12px;
	}
	.top {
		display: flex;
		align-items: flex-start;
		gap: 12px;
	}
	.main {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
		text-shadow: 0 1px 12px rgba(0, 0, 0, 0.28);
	}
	.place {
		all: unset;
		cursor: pointer;
		font-size: 13px;
		font-weight: 600;
		opacity: 0.88;
		align-self: flex-start;
	}
	.place:focus-visible {
		outline: 2px solid #fff;
		outline-offset: 2px;
		border-radius: 4px;
	}
	.temp {
		font-size: 54px;
		font-weight: 500;
		line-height: 1;
		letter-spacing: -0.04em;
	}
	.compact .temp {
		font-size: 42px;
	}
	.large .temp {
		font-size: 72px;
	}
	.sky {
		font-size: 15px;
		font-weight: 700;
	}
	.large .sky {
		font-size: 18px;
	}
	.range {
		font-size: 13px;
		opacity: 0.9;
	}
	.side {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: 6px;
		font-size: 12px;
		text-align: right;
		text-shadow:
			0 1px 3px rgba(0, 0, 0, 0.45),
			0 0 14px rgba(0, 0, 0, 0.35);
		/* Below the sun or moon in the corner. */
		margin-top: 46px;
	}
	.large .side {
		margin-top: 66px;
	}
	.glass {
		-webkit-backdrop-filter: blur(8px);
		backdrop-filter: blur(8px);
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 4px 10px;
		border-radius: 999px;
		font-weight: 700;
		text-shadow: none;
	}
	.chip.small {
		align-self: flex-start;
		margin-top: auto;
		font-size: 12px;
		padding: 3px 9px;
	}
	.chip svg {
		width: 13px;
		height: 13px;
	}
	.ribbon {
		align-self: flex-start;
		max-width: 100%;
		box-sizing: border-box;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 10px 4px 8px;
		border-radius: 999px;
		background: #c62828;
		color: #fff;
		font-size: 12px;
		font-weight: 700;
		box-shadow: 0 2px 8px rgba(120, 0, 0, 0.35);
	}
	.ribbon.watch {
		background: #b86a00;
	}
	.ribbon span {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.ribbon svg {
		flex: none;
		width: 14px;
		height: 14px;
		fill: none;
		stroke: currentColor;
		stroke-width: 2.4;
		stroke-linecap: round;
		stroke-linejoin: round;
	}
	svg path {
		fill: none;
		stroke: currentColor;
		stroke-width: 2;
		stroke-linecap: round;
		stroke-linejoin: round;
	}
	.hours {
		margin-top: auto;
		display: grid;
		border-radius: 14px;
		padding: 8px 6px;
		text-align: center;
	}
	.hour {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 3px;
		font-size: 12px;
	}
	.hour .t {
		opacity: 0.85;
	}
	.hour svg,
	.day svg {
		width: 18px;
		height: 18px;
	}
	.days {
		display: flex;
		flex-direction: column;
		border-radius: 14px;
		padding: 6px 12px;
	}
	.day {
		display: grid;
		grid-template-columns: 48px 22px 1fr 34px minmax(60px, 110px) 34px;
		align-items: center;
		gap: 8px;
		font-size: 13px;
		padding: 4px 0;
	}
	.day .what {
		opacity: 0.85;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		text-transform: capitalize;
	}
	.day .lo {
		opacity: 0.75;
		text-align: right;
	}
	.bar {
		height: 5px;
		border-radius: 9px;
		background: rgba(255, 255, 255, 0.22);
		position: relative;
	}
	.bar span {
		position: absolute;
		top: 0;
		bottom: 0;
		border-radius: 9px;
		background: linear-gradient(90deg, #8fd3ff, #ffd36b, #ff9a4d);
	}

	/* The scene. */
	.sun {
		position: absolute;
		right: -6px;
		top: -26px;
		width: 82px;
		height: 82px;
	}
	.compact .sun {
		right: -16px;
		top: -16px;
		width: 74px;
		height: 74px;
	}
	.large .sun {
		right: 24px;
		top: -28px;
		width: 110px;
		height: 110px;
	}
	.disc {
		position: absolute;
		inset: 0;
		border-radius: 50%;
		background: radial-gradient(circle at 40% 38%, #fffbe0, #ffe37a 55%, #ffc93c);
		box-shadow: 0 0 46px 16px rgba(255, 214, 90, 0.5);
	}
	.rays {
		position: absolute;
		inset: -45%;
		width: 190%;
		height: 190%;
		animation: spin 70s linear infinite;
	}
	.rays path {
		stroke: rgba(255, 236, 170, 0.55);
		stroke-width: 1.6;
	}
	.moon {
		position: absolute;
		right: 34px;
		top: 18px;
		width: 40px;
		height: 40px;
		border-radius: 50%;
		box-shadow: 10px 7px 0 0 #f3eed8;
		filter: drop-shadow(0 0 10px rgba(243, 238, 216, 0.45));
	}
	.compact .moon {
		right: 22px;
		width: 32px;
		height: 32px;
	}
	.large .moon {
		right: 70px;
		width: 52px;
		height: 52px;
	}
	.star {
		position: absolute;
		border-radius: 50%;
		background: #fff;
		animation: twinkle 3.4s ease-in-out infinite;
	}
	.lane {
		position: absolute;
		left: 0;
		width: 100%;
		animation: drift linear infinite;
	}
	.fall {
		position: absolute;
		inset: -30% -20%;
	}
	.drop {
		position: absolute;
		top: 0;
		height: 100%;
		width: 1.6px;
		animation: fall linear infinite;
	}
	.flake {
		position: absolute;
		top: 0;
		height: 100%;
		animation: snow linear infinite;
	}
	.flake::before {
		content: '';
		display: block;
		width: var(--s);
		height: var(--s);
		border-radius: 50%;
		background: #fff;
		box-shadow: 0 0 4px rgba(255, 255, 255, 0.8);
	}
	.flash {
		position: absolute;
		inset: 0;
		background: #ebe6ff;
		opacity: 0;
		animation: flash 7s infinite;
	}
	.bolt {
		position: absolute;
		left: 62%;
		top: 10%;
		width: 26px;
		opacity: 0;
		animation: bolt 7s infinite;
		filter: drop-shadow(0 0 8px rgba(255, 240, 150, 0.9));
	}
	.bolt path {
		fill: #fff6b3;
		stroke: none;
	}
	.mist {
		position: absolute;
		left: -40%;
		width: 180%;
		background: linear-gradient(90deg, transparent, rgba(255, 255, 255, 0.45) 30%, rgba(255, 255, 255, 0.25) 60%, transparent);
		filter: blur(8px);
		animation: mist ease-in-out infinite alternate;
	}
	.still *,
	.still *::before {
		animation-play-state: paused !important;
	}
	@media (prefers-reduced-motion: reduce) {
		.card *,
		.card *::before {
			animation-play-state: paused !important;
		}
		.card {
			transition: none;
		}
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	@keyframes twinkle {
		0%,
		100% {
			opacity: 0.2;
		}
		50% {
			opacity: 1;
		}
	}
	@keyframes drift {
		from {
			transform: translateX(-75%);
		}
		to {
			transform: translateX(105%);
		}
	}
	@keyframes fall {
		from {
			transform: translateY(-25%);
		}
		to {
			transform: translateY(100%);
		}
	}
	@keyframes snow {
		0% {
			transform: translate(0, -12%);
		}
		25% {
			transform: translate(9px, 14%);
		}
		50% {
			transform: translate(-7px, 42%);
		}
		75% {
			transform: translate(8px, 70%);
		}
		100% {
			transform: translate(0, 100%);
		}
	}
	@keyframes flash {
		0%,
		84%,
		100% {
			opacity: 0;
		}
		85% {
			opacity: 0.6;
		}
		86.5% {
			opacity: 0;
		}
		88% {
			opacity: 0.4;
		}
		90% {
			opacity: 0;
		}
	}
	@keyframes bolt {
		0%,
		84%,
		91%,
		100% {
			opacity: 0;
		}
		85%,
		89% {
			opacity: 1;
		}
	}
	@keyframes mist {
		from {
			transform: translateX(-10%);
		}
		to {
			transform: translateX(10%);
		}
	}
</style>
