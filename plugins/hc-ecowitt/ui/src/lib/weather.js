// What hc-ecowitt's devices report, put the way a person reads it.
// The plugin converts every reading to homeCore's units — °C, m/s, hPa, mm,
// km, W/m² — whichever way it hears the gateway. The host gives these screens
// no unit preference, so they show those units as they are.

/** @typedef {import('../../../../../sdk/ui/index.js').Device} Device */
/** @typedef {import('../../../../../sdk/ui/index.js').Host} Host */

/** Use the app's look in a component's shadow root. */
export function adopt(el, host) {
	const root = el?.shadowRoot;
	if (!root || !host) return;
	const sheet = host.theme.sheet;
	if (!root.adoptedStyleSheets.includes(sheet))
		root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
}

export const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined);

/** `52.3`, or a dash. */
export function fmt(v, digits = 1) {
	const n = num(v);
	return n === undefined ? '—' : n.toFixed(digits).replace(/\.0+$/, '');
}

const POINTS = ['N', 'NNE', 'NE', 'ENE', 'E', 'ESE', 'SE', 'SSE', 'S', 'SSW', 'SW', 'WSW', 'W', 'WNW', 'NW', 'NNW'];
/** 213° → "SSW". */
export const compass = (deg) =>
	num(deg) === undefined ? '' : POINTS[Math.round((((deg % 360) + 360) % 360) / 22.5) % 16];

/** How long ago, briefly: "8 s", "4 min", "2 h". */
export function ago(iso, now = Date.now()) {
	const t = Date.parse(iso ?? '');
	if (!Number.isFinite(t)) return 'never';
	const s = Math.max(0, Math.round((now - t) / 1000));
	if (s < 60) return `${s} s ago`;
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s < 86400) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/** Stale after three missed uploads at the usual minute. */
export const stale = (iso, now = Date.now()) => now - Date.parse(iso ?? '') > 3.5 * 60_000;

export const DEG = '°C';

/** A sensor's battery: `battery_pct` where it is a level, `voltage` where it
 *  is a cell voltage, and only `battery_low` where it is a 0/1 flag. */
export function battery(a) {
	if (a.battery_kind === undefined && a.battery_low === undefined) return { text: '—', tone: '' };
	const low = a.battery_low === true;
	let text = low ? 'Low' : 'OK';
	if (num(a.battery_pct) !== undefined) text = `${fmt(a.battery_pct, 0)} %${low ? ' · low' : ''}`;
	else if (num(a.voltage) !== undefined) text = `${fmt(a.voltage, 2)} V${low ? ' · low' : ''}`;
	return { text, tone: low ? 'bad' : 'good' };
}

/** The rain gauge to show: the piezo one where there is one, else the bucket. */
export const rainGauge = (devices) =>
	devices.find((d) => d.device_type === 'rain_sensor' && d.device_id.endsWith('_piezo_rain')) ??
	devices.find((d) => d.device_type === 'rain_sensor');

/** Raining now: the piezo sensor says so, or the rate is above nothing. */
export const isRaining = (r) => r.raining === true || Number(r.rain_rate_mm_hr ?? 0) > 0;

const KIND = {
	weather_station: 'Weather station',
	temperature_sensor: 'Temperature',
	rain_sensor: 'Rain',
	lightning_sensor: 'Lightning',
	air_quality_sensor: 'Air quality',
	soil_sensor: 'Soil',
	leak_sensor: 'Leak',
	leaf_sensor: 'Leaf wetness',
	ec_sensor: 'Soil EC',
	level_sensor: 'Water level',
	gateway: 'Gateway'
};
export const kindOf = (t) => KIND[t] ?? (t ? t.replace(/_/g, ' ') : 'Sensor');

/** The one reading that says most about a sensor. */
export function reading(d) {
	const a = d.attributes;
	switch (d.device_type) {
		case 'weather_station':
			return `${fmt(a.temperature)}${DEG} · ${fmt(a.humidity, 0)}% · wind ${fmt(a.wind_speed)} m/s`;
		case 'temperature_sensor':
			return `${fmt(a.temperature)}${DEG}${num(a.humidity) !== undefined ? ` · ${fmt(a.humidity, 0)}%` : ''}`;
		case 'rain_sensor': {
			const rate = a.rain_rate_mm_hr;
			return `${fmt(a.rain_daily_mm)} mm today${num(rate) ? ` · ${fmt(rate)} mm/h now` : ''}`;
		}
		case 'lightning_sensor':
			return `${fmt(a.strike_count, 0)} strikes · nearest ${fmt(a.distance_km, 0)} km`;
		case 'air_quality_sensor':
			return a.co2 !== undefined ? `CO₂ ${fmt(a.co2, 0)} ppm` : `PM2.5 ${fmt(a.pm25, 0)}`;
		case 'soil_sensor':
			return `${fmt(a.soil_moisture_pct, 0)}% moisture`;
		case 'leaf_sensor':
			return `${fmt(a.leaf_wetness_pct, 0)}% wet`;
		case 'leak_sensor':
			return a.water_detected === true ? 'Water detected' : a.water_detected === false ? 'Dry' : '—';
		default:
			return '';
	}
}

/** Which way the pressure is going, from its last few hours. */
export function trend(points) {
	const xs = points.map((p) => Number(p.value)).filter(Number.isFinite);
	if (xs.length < 2) return '';
	const d = xs[xs.length - 1] - xs[0];
	// hPa: under 0.7 over the window is noise, not weather.
	return Math.abs(d) < 0.7 ? 'steady' : d > 0 ? 'rising' : 'falling';
}

/** The UV index, in words. */
export function uvWords(uv) {
	const n = num(uv);
	if (n === undefined) return '';
	return n < 3 ? 'low' : n < 6 ? 'moderate' : n < 8 ? 'high' : n < 11 ? 'very high' : 'extreme';
}
