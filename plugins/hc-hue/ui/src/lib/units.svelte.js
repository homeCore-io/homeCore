// The units the person reads in, from the app (`host.units`, app 0.1.21 and
// later). Hue publishes °C, always; an older app gives no units, and the
// screens show °C as it is.

/** @typedef {import('../../../../../sdk/ui/index.js').Host} Host */

/** The person's choice, read so a screen redraws when they change it. */
let chosen = $state(undefined);
/** @type {import('../../../../../sdk/ui/index.js').Units | undefined} */
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

/** A temperature in °C as the person reads it: `{ n: '70.7', unit: '°F' }`. */
export function temperature(c) {
	void chosen;
	const s = given?.show(c, '°C');
	return s ? { n: s.value.toFixed(1), unit: s.unit ?? '°C' } : { n: c.toFixed(1), unit: '°C' };
}
