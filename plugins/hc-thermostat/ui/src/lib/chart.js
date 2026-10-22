// The Today chart's arithmetic, apart so it can be tested.

/**
 * The periods the switch was on, from the history of `actuator_state`
 * (oldest first): `[{from, to}]` in ms, clipped to `start`…`now`. What it
 * was before the first report since `start` isn't known, so a first "off"
 * shades nothing before it — unless `onSince` (the plugin's `on_since`) says
 * the run going now began earlier.
 */
export function onRuns(rows, start, now, onSince) {
	const pts = rows
		.map((r) => ({ at: Date.parse(r.at), on: r.value === true }))
		.filter((p) => Number.isFinite(p.at))
		.sort((a, b) => a.at - b.at);
	const out = [];
	let since = null;
	for (const p of pts) {
		if (p.on && since === null) since = Math.max(p.at, start);
		else if (!p.on && since !== null) {
			if (p.at > start) out.push({ from: since, to: p.at });
			since = null;
		}
	}
	if (since !== null) out.push({ from: since, to: now });
	// On now since before the history shows it: from then (or midnight).
	if (Number.isFinite(onSince) && (since === null || onSince < since)) {
		const from = Math.max(onSince, start);
		const last = out.at(-1);
		if (last && last.to === now) last.from = Math.min(last.from, from);
		else out.push({ from, to: now });
	}
	return out.filter((r) => r.to > r.from);
}

/** The y range: every point and the band, with a little room. */
export function yRange(points, band) {
	const vs = points.map((p) => p.v).concat([band.low, band.high]);
	let min = Math.min(...vs);
	let max = Math.max(...vs);
	const pad = Math.max(0.4, (max - min) * 0.15);
	min -= pad;
	max += pad;
	return { min, max };
}

/** An SVG path through the points, held flat between readings (a sensor
 * reports a change, not a slope). */
export function pathOf(points, x, y) {
	if (!points.length) return '';
	let d = `M${x(points[0].at).toFixed(1)},${y(points[0].v).toFixed(1)}`;
	for (let i = 1; i < points.length; i++) {
		const p = points[i];
		d += `H${x(p.at).toFixed(1)}V${y(p.v).toFixed(1)}`;
	}
	return d;
}
