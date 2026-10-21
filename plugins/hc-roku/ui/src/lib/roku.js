// What the Roku screens share: the Rokus, their channels (with the icons the
// plugin keeps), what each is showing in words, and the commands — all from
// the plugin's published state, through the `host` each screen is handed.

/** Give a shadow root the app's parts (host.theme.sheet) beside its own. */
export function adopt(el, host) {
	const root = el?.shadowRoot;
	if (!root || !host) return;
	const sheet = host.theme.sheet;
	if (!root.adoptedStyleSheets.includes(sheet))
		root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
}

export const num = (v) => {
	const n = typeof v === 'string' && v.trim() !== '' ? Number(v) : v;
	return typeof n === 'number' && Number.isFinite(n) ? n : undefined;
};

const attrs = (d) => d?.attributes ?? {};

/** The plugin's Rokus, by name. */
export function rokusOf(devices, label = (d) => d.name) {
	return devices
		.filter((d) => d.device_type === 'media_player')
		.sort((a, b) => label(a).localeCompare(label(b)));
}

/** Its channels, in the Roku's own order (its home-screen arrangement). */
export function appsOf(d) {
	const list = attrs(d).available_apps;
	return Array.isArray(list) ? list.filter((a) => a && typeof a.id === 'string') : [];
}

/** Its TV inputs (HDMI 1, Live TV…), by the names given them on the TV. */
export function inputsOf(d) {
	const list = attrs(d).available_inputs;
	return Array.isArray(list) ? list.filter((a) => a && typeof a.id === 'string') : [];
}

/** Most used first — opened from homeCore most often — then the Roku's own order. */
export function mostUsed(d, n = 6) {
	return appsOf(d)
		.map((a, i) => ({ a, i }))
		.sort((x, y) => (num(y.a.launches) ?? 0) - (num(x.a.launches) ?? 0) || x.i - y.i)
		.slice(0, n)
		.map((x) => x.a);
}

/** A–Z, for finding one. */
export const az = (apps) => [...apps].sort((a, b) => a.name.localeCompare(b.name));

/** Narrowed to what was typed, ignoring case. */
export function search(apps, q) {
	const t = (q ?? '').trim().toLowerCase();
	return t ? apps.filter((a) => a.name.toLowerCase().includes(t)) : apps;
}

/** The name on a small tile: "Plex - Free Movies & TV" → "Plex". */
export const short = (name) =>
	String(name ?? '')
		.replace(/ - .*$/, '')
		.replace(/^Roku \| /, '');

const PALETTE = [
	'#3d4a5c',
	'#4a2a72',
	'#1f5a85',
	'#2f5a27',
	'#7a3a05',
	'#5d6470',
	'#8a2b4a',
	'#2b5d5a',
	'#5a4a1f',
	'#1e2126'
];
/** A channel's own colour, for its tile until its icon arrives. */
export const tint = (name) =>
	PALETTE[
		[...String(name ?? '')].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 997, 7) % PALETTE.length
	];

/** Where its icon is, from the plugin's kept files — null without one. */
export function iconUrl(hc, app) {
	if (!app?.icon || typeof hc?.fileUrl !== 'function') return null;
	return hc.fileUrl(app.icon);
}

export const isTv = (d) => attrs(d).is_tv === true;
export const isOn = (d) => attrs(d).on === true;
export const canFindRemote = (d) => attrs(d).supports_find_remote === true;
export const isPlaying = (d) => attrs(d).state === 'playing' || attrs(d).state === 'buffering';
export const isPaused = (d) => attrs(d).state === 'paused';
/** The app on screen, from its catalogue entry when it has one. */
export function appOn(d) {
	const id = attrs(d).app_id;
	if (typeof id !== 'string') return null;
	return (
		appsOf(d).find((a) => a.id === id) ??
		inputsOf(d).find((a) => a.id === id) ?? { id, name: attrs(d).app_name ?? id }
	);
}

/**
 * What it is doing, for a screen to say: `offline`, `off`, `home`, `input`
 * (an HDMI input or live TV), or `app`; with the app, whether it plays, and
 * the position.
 */
export function nowOf(d) {
	const a = attrs(d);
	if (!d || d.available === false) return { kind: 'offline', name: '' };
	if (a.on === false) return { kind: 'off', name: '' };
	if (a.on !== true) return { kind: 'offline', name: '' };
	const app = appOn(d);
	if (!app) return { kind: 'home', name: 'Home' };
	if (a.is_tv_input === true)
		return { kind: 'input', name: app.name, app, live: a.tv_channel ?? null };
	return { kind: 'app', name: app.name, app, playing: isPlaying(d), paused: isPaused(d) };
}

/** 3:45 from 225; 1:02:40 past an hour. */
export function clock(secs) {
	const s = Math.max(0, Math.floor(num(secs) ?? 0));
	const h = Math.floor(s / 3600);
	const m = Math.floor(s / 60) % 60;
	const ss = String(s % 60).padStart(2, '0');
	return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

/**
 * Where the video is now: the last reported position, moved on by the time
 * since it was true while it plays. The plugin says when
 * (`media_position_updated_at`) and publishes it again only when it jumps.
 */
export function positionNow(d, now = Date.now()) {
	const a = attrs(d);
	const pos = num(a.media_position);
	if (pos === undefined) return null;
	const dur = num(a.media_duration) ?? 0;
	const at = Date.parse(a.media_position_updated_at ?? '');
	let p = pos;
	if (isPlaying(d) && Number.isFinite(at)) p = pos + Math.max(0, now - at) / 1000;
	return { pos: dur ? Math.min(p, dur) : p, dur };
}

/** One line for a Roku, as the cards and the page say it. */
export function stateLine(d, now = Date.now()) {
	const n = nowOf(d);
	if (n.kind === 'offline') return 'Not answering';
	if (n.kind === 'off') return 'Off';
	if (n.kind === 'home') return 'On · Home';
	if (n.kind === 'input') return `On · ${n.name}`;
	const where = positionNow(d, now);
	const at = where ? ` · ${clock(where.pos)}` : '';
	if (n.playing) return `Playing ${n.name}${at}`;
	if (n.paused) return `Paused · ${n.name}${at}`;
	return `On · ${n.name}`;
}

/** "A", "A and B", "A, B and C". */
export function listWords(items) {
	if (items.length <= 1) return items.join('');
	return `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`;
}

/** "just now", "3 min ago", from an ISO time. */
export function ago(iso, now = Date.now()) {
	const t = Date.parse(iso ?? '');
	if (!Number.isFinite(t)) return '';
	const s = Math.max(0, (now - t) / 1000);
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s < 86400) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/** "every 15 min" from 900. */
export function every(secs) {
	const s = num(secs) ?? 0;
	if (s >= 3600 && s % 3600 === 0) return `every ${s / 3600} h`;
	return `every ${Math.round(s / 60)} min`;
}

/** Play or pause, whichever it isn't doing. */
export const playPause = (hc, d) => hc.command(d.device_id, { action: 'play_pause' });
export const power = (hc, d) => hc.command(d.device_id, { on: !isOn(d) });
export const openApp = (hc, d, app) =>
	hc.command(d.device_id, { action: 'launch_app', app: app.id });
export const switchInput = (hc, d, input) =>
	hc.command(d.device_id, { action: 'select_source', source: input.id });
export const press = (hc, d, key, count = 1) =>
	hc.command(d.device_id, count > 1 ? { action: 'key', key, count } : { action: 'key', key });

/** Where the plugin's own page and settings are, in the app. */
export const pageHref = (hc) => `/settings/plugins/${encodeURIComponent(hc.pluginId)}`;
export const settingsHref = (hc) => `${pageHref(hc)}?tab=settings`;
