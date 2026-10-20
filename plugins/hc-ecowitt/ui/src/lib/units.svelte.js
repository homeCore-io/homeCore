// The units the person reads in, from the app (`host.units`, app 0.1.21 and
// later): °F or °C, and inHg, mph, in — or hPa, m/s, mm. The plugin publishes
// homeCore's own (°C, hPa, m/s, mm, mm/h, km); an older app gives no units,
// and the screens show those as they are.

/** @typedef {import('../../../../../sdk/ui/index.js').Host} Host */
/** @typedef {import('../../../../../sdk/ui/index.js').Units} Units */

export const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined);

/** `52.3`, or a dash. */
export function fmt(v, digits = 1) {
	const n = num(v);
	return n === undefined ? '—' : n.toFixed(digits).replace(/\.0+$/, '');
}

/** The person's choice, read so a screen redraws when they change it. */
let chosen = $state(undefined);
/** @type {Units | undefined} */
let given;

/**
 * Hear the person's units through `host` — in each element's `$effect`;
 * returns the way to stop.
 * @param {Host | undefined} host
 */
export function follow(host) {
	const units = host?.units;
	if (!units) return undefined;
	given = units;
	return units.subscribe((x) => (chosen = x));
}

function shown(v, unit) {
	void chosen;
	return given?.show(v, unit);
}

/** A reading's number as the person reads it — "70.7", "29.92" — or a dash. */
export function n(v, unit, digits = 1) {
	const x = num(v);
	if (x === undefined) return '—';
	const s = shown(x, unit);
	// Converted, it comes rounded as its unit reads; else the screen's own way.
	return s && s.unit !== unit ? String(s.value) : fmt(x, digits);
}

/** The unit a reading in `unit` is shown in: "°F" for "°C", "inHg" for "hPa". */
export const u = (unit) => shown(0, unit)?.unit ?? unit;

/** A history's points in the person's units, for drawing. */
export function series(points, unit) {
	void chosen;
	if (!given) return points;
	return points.map((p) => {
		const x = Number(p.value);
		return Number.isFinite(x) ? { ...p, value: given.show(x, unit).value } : p;
	});
}
