// What the screens share: colour arithmetic, a room's scenes, names.

/** Give a shadow root the app's parts (host.theme.sheet) beside its own. */
export function adopt(el, host) {
	const root = el?.shadowRoot;
	if (!root || !host) return;
	const sheet = host.theme.sheet;
	if (!root.adoptedStyleSheets.includes(sheet))
		root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
}

export const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined);

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const hex2 = (v) => Math.round(clamp(v, 0, 1) * 255).toString(16).padStart(2, '0');
const toHex = (r, g, b) => `#${hex2(r)}${hex2(g)}${hex2(b)}`;
const gamma = (c) => (c <= 0.0031308 ? 12.92 * c : 1.055 * Math.pow(c, 1 / 2.4) - 0.055);
const linear = (c) => (c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4));

/** A CIE xy point as the colour a screen draws, at full brightness. */
export function xyToHex(x, y) {
	y = Math.max(y, 0.0001);
	const Y = 1;
	const X = (Y / y) * x;
	const Z = (Y / y) * (1 - x - y);
	let r = X * 1.656492 - Y * 0.354851 - Z * 0.255038;
	let g = -X * 0.707196 + Y * 1.655397 + Z * 0.036152;
	let b = X * 0.051713 - Y * 0.121364 + Z * 1.01153;
	const m = Math.max(r, g, b);
	if (m > 1) [r, g, b] = [r / m, g / m, b / m];
	return toHex(gamma(Math.max(r, 0)), gamma(Math.max(g, 0)), gamma(Math.max(b, 0)));
}

/** A colour from the screen as the xy point a Hue light takes. */
export function hexToXy(hex) {
	const n = parseInt(hex.slice(1), 16);
	const [r, g, b] = [(n >> 16) & 255, (n >> 8) & 255, n & 255].map((c) => linear(c / 255));
	const X = r * 0.664511 + g * 0.154324 + b * 0.162028;
	const Y = r * 0.283881 + g * 0.668433 + b * 0.047685;
	const Z = r * 0.000088 + g * 0.07231 + b * 0.986039;
	const s = X + Y + Z || 1;
	return { x: +(X / s).toFixed(4), y: +(Y / s).toFixed(4) };
}

/** Hue (0–360) and saturation (0–1) as a colour a screen draws. */
export function hsToHex(h, s) {
	const f = (n) => {
		const k = (n + h / 60) % 6;
		return 1 - s * Math.max(0, Math.min(k, 4 - k, 1));
	};
	return toHex(f(5), f(3), f(1));
}

/** The hue and saturation of a colour, for placing it on the wheel. */
export function hexToHs(hex) {
	const n = parseInt(hex.slice(1), 16);
	const [r, g, b] = [(n >> 16) & 255, (n >> 8) & 255, n & 255].map((c) => c / 255);
	const max = Math.max(r, g, b);
	const min = Math.min(r, g, b);
	const d = max - min;
	let h = 0;
	if (d) {
		if (max === r) h = ((g - b) / d) % 6;
		else if (max === g) h = (b - r) / d + 2;
		else h = (r - g) / d + 4;
	}
	return { h: (h * 60 + 360) % 360, s: max ? d / max : 0 };
}

/** A white of `kelvin` as a screen draws it. */
export function kelvinToHex(kelvin) {
	const t = clamp(kelvin, 1000, 40000) / 100;
	const r = t <= 66 ? 255 : 329.698727446 * Math.pow(t - 60, -0.1332047592);
	const g = t <= 66 ? 99.4708025861 * Math.log(t) - 161.1195681661 : 288.1221695283 * Math.pow(t - 60, -0.0755148492);
	const b = t >= 66 ? 255 : t <= 19 ? 0 : 138.5177312231 * Math.log(t - 10) - 305.0447927307;
	return toHex(r / 255, g / 255, b / 255);
}

/** Whether a light is showing a white rather than a colour: the plugin
 * says which (`color_mode`). */
export const isWhite = (a) =>
	a?.color_mode ? a.color_mode === 'color_temp' : !a?.supports_color_xy || !a?.color_xy;

/** A light's swatch: its gradient, colour or white. */
export function swatchOf(a) {
	if (!a?.on) return 'var(--track-off, #cfc8ba)';
	const g =
		a.color_mode !== 'color_temp' && a.color_mode !== 'color' && Array.isArray(a.gradient_points) && a.gradient_points.length > 1
			? a.gradient_points
			: null;
	if (g) return `linear-gradient(180deg, ${g.map((p) => xyToHex(p.x, p.y)).join(', ')})`;
	if (!isWhite(a) && a.color_xy) return xyToHex(a.color_xy.x, a.color_xy.y);
	return num(a.color_temp) ? kelvinToHex(a.color_temp) : '#ffd8a3';
}

/** One colour for a light's glow: its gradient's first point, else its swatch. */
export function glowOf(a) {
	if (!a?.on) return 'transparent';
	const g = a.color_mode === 'gradient' && Array.isArray(a.gradient_points) ? a.gradient_points[0] : null;
	return g ? xyToHex(g.x, g.y) : swatchOf(a);
}

const words = { no_effect: 'None', underwater: 'Underwater' };
/** "candle" → "Candle". */
export const effectName = (e) => words[e] ?? (e ? e[0].toUpperCase() + e.slice(1).replaceAll('_', ' ') : '');

/** The scenes of one Hue room or zone (its rid), in a stable order: the
 * colourful ones first, then whites, each by name. */
export function scenesOf(devices, groupRid) {
	if (!groupRid) return [];
	return devices
		.filter((d) => d.device_type === 'scene' && d.attributes.group_rid === groupRid)
		.sort((a, b) => {
			const pa = (a.attributes.palette ?? []).length > 1 ? 0 : 1;
			const pb = (b.attributes.palette ?? []).length > 1 ? 0 : 1;
			return pa - pb || a.name.localeCompare(b.name);
		});
}

/** Every room and zone the plugin's scenes belong to, by rid. */
export function groupsOf(devices) {
	const by = new Map();
	for (const d of devices) {
		if (d.device_type !== 'scene') continue;
		const rid = d.attributes.group_rid;
		if (!rid || by.has(rid)) continue;
		by.set(rid, { rid, name: d.attributes.group_name ?? 'Room', kind: d.attributes.group_kind ?? 'room' });
	}
	return [...by.values()].sort((a, b) => (a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === 'room' ? -1 : 1));
}

/** The scene running in a room, if one is. */
export const runningIn = (devices, groupRid) =>
	scenesOf(devices, groupRid).find((s) => s.attributes.active === true);

/** The palette of a scene as a CSS background for a strip of swatches. */
export const paletteOf = (scene) => {
	const p = scene?.attributes?.palette;
	return Array.isArray(p) && p.length ? p : ['#ffd8a3'];
};

/** "3 min ago", from an ISO time. */
export function ago(iso) {
	const t = Date.parse(iso ?? '');
	if (!Number.isFinite(t)) return '';
	const s = Math.max(0, (Date.now() - t) / 1000);
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s < 86400) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/** A fade time for a person: "instantly", "2 s", "1 min". */
export function fadeWords(ms) {
	if (!ms) return 'instantly';
	if (ms < 60000) return `${Math.round(ms / 100) / 10} s`;
	return `${Math.round(ms / 60000)} min`;
}

export const FADES = [0, 400, 2000, 10000, 60000, 600000, 1800000];
