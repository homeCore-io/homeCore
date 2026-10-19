// What hc-ecowitt's devices report, put the way a person reads it.
// Units are what the gateway's "Customized" upload sends: °F, mph, inHg, in.

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

/** A sensor's battery, as the plugin classifies it (`battery_kind`). */
export function battery(a) {
	if (a.battery_kind === undefined && a.battery_low === undefined) return { text: '—', tone: '' };
	const low = a.battery_low === true;
	let text = low ? 'Low' : 'OK';
	if (a.battery_kind === 'voltage' && num(a.battery_volts) !== undefined)
		text = `${fmt(a.battery_volts, 2)} V${low ? ' · low' : ''}`;
	else if (a.battery_kind === 'level' && num(a.battery) !== undefined)
		text = `${fmt(a.battery, 0)}${a.battery <= 5 ? ' / 5' : ' %'}${low ? ' · low' : ''}`;
	return { text, tone: low ? 'bad' : 'good' };
}

const KIND = {
	weather_station: 'Weather station',
	temperature_sensor: 'Temperature',
	rain_sensor: 'Rain',
	lightning_sensor: 'Lightning',
	air_quality_sensor: 'Air quality',
	soil_sensor: 'Soil',
	leak_sensor: 'Leak',
	gateway: 'Gateway'
};
export const kindOf = (t) => KIND[t] ?? (t ? t.replace(/_/g, ' ') : 'Sensor');

/** The one reading that says most about a sensor. */
export function reading(d) {
	const a = d.attributes;
	const deg = a.temperature_unit ?? '°F';
	switch (d.device_type) {
		case 'weather_station':
			return `${fmt(a.temperature)}${deg} · ${fmt(a.humidity, 0)}% · wind ${fmt(a.wind_speed)} mph`;
		case 'temperature_sensor':
			return `${fmt(a.temperature)}${deg}${num(a.humidity) !== undefined ? ` · ${fmt(a.humidity, 0)}%` : ''}`;
		case 'rain_sensor': {
			const rate = a.rain_rate_piezo ?? a.rain_rate;
			const day = a.rain_day_piezo ?? a.rain_day;
			return `${fmt(day, 2)} in today${num(rate) ? ` · ${fmt(rate, 2)} in/h now` : ''}`;
		}
		case 'lightning_sensor':
			return `${fmt(a.strike_count, 0)} strikes · nearest ${fmt(a.distance, 0)} mi`;
		case 'air_quality_sensor':
			return a.co2 !== undefined ? `CO₂ ${fmt(a.co2, 0)} ppm` : `PM2.5 ${fmt(a.pm25, 0)}`;
		default:
			return '';
	}
}

/** Which way the pressure is going, from its last few hours. */
export function trend(points) {
	const xs = points.map((p) => Number(p.value)).filter(Number.isFinite);
	if (xs.length < 2) return '';
	const d = xs[xs.length - 1] - xs[0];
	return Math.abs(d) < 0.02 ? 'steady' : d > 0 ? 'rising' : 'falling';
}

/** The UV index, in words. */
export function uvWords(uv) {
	const n = num(uv);
	if (n === undefined) return '';
	return n < 3 ? 'low' : n < 6 ? 'moderate' : n < 8 ? 'high' : n < 11 ? 'very high' : 'extreme';
}
