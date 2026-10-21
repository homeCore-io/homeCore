// What hc-caseta's screens share beyond hc-lutron's helpers (./lutron.js,
// copied verbatim): words for the bridge, its lights and scenes, and which
// automations use a scene.

/** "On/off", "Dimmer" — what a device's configured kind makes it. */
export const worksAs = (kind) =>
	({
		switch: 'On/off',
		dimmer: 'Dimmer',
		shade: 'Shade',
		fan_control: 'Fan',
		occupancy_sensor: 'Occupancy sensor',
		pico: 'Pico remote'
	})[kind] ??
	kind ??
	'—';

/** A light's state now, in a word or two. */
export function nowWords(d) {
	if (!d) return '';
	if (d.available === false) return 'Offline';
	const a = d.attributes ?? {};
	if (typeof a.position === 'number')
		return a.position > 0 ? `${Math.round(a.position)} % open` : 'Closed';
	if (a.on !== true) return a.on === false ? 'Off' : '—';
	if (typeof a.brightness_pct === 'number') return `On · ${Math.round(a.brightness_pct)} %`;
	if (typeof a.speed === 'string' && a.speed !== 'off') return `On · ${a.speed}`;
	return 'On';
}

/** The session with the bridge, as the status pill says it. */
export function linkWords(status) {
	switch (status?.state) {
		case 'connected':
			return { text: 'Connected to the Smart Bridge', good: true };
		case 'bad_credentials':
			return { text: 'The bridge refused the login', good: false };
		case 'not_configured':
			return { text: 'No bridge address set', good: false };
		case 'connecting':
			return { text: 'Connecting to the Smart Bridge', good: false };
		default:
			return { text: 'Not connected', good: false };
	}
}

/** "2:14 PM" today, else "Oct 3, 2:14 PM". */
export function clock(iso) {
	const t = new Date(iso ?? '');
	if (!Number.isFinite(t.getTime())) return '';
	const time = t.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
	const today = new Date().toDateString() === t.toDateString();
	return today ? time : `${t.toLocaleDateString([], { month: 'short', day: 'numeric' })}, ${time}`;
}

/** Every rule whose steps set `deviceId` — a scene it runs, a Pico it presses. */
export function usesOf(rules, deviceId) {
	const names = (v) => {
		if (!v || typeof v !== 'object') return false;
		if (Array.isArray(v)) return v.some(names);
		if (v.device_id === deviceId) return true;
		if (Array.isArray(v.device_ids) && v.device_ids.includes(deviceId)) return true;
		return Object.values(v).some(names);
	};
	return rules.filter((r) => names(r.actions));
}

/** "Used by 2 automations". */
export const usedByWords = (n) =>
	n === 0 ? 'Not used by an automation' : `Used by ${n} automation${n === 1 ? '' : 's'}`;

/** A Pico's buttons top to bottom, as the Pico has them: On, Raise,
 * Favorite, Lower, Off; anything else after, by number. */
export const PLACE = { 2: 0, 5: 1, 3: 2, 6: 3, 4: 4 };
export const onThePico = (buttons) =>
	[...buttons].sort(
		(x, y) => (PLACE[x.number] ?? 10 + x.number) - (PLACE[y.number] ?? 10 + y.number)
	);

/** A key drawn smaller on the Pico: Favorite, Raise and Lower. */
export const smallKey = (buttons, n) => buttons.length > 2 && (n === 3 || n === 5 || n === 6);

/** What the bridge said about something nobody set up, in words. */
export function heardWords(h) {
	if (h.what === 'zone') return `Zone ${h.id}`;
	if (h.what === 'scene') return `Scene on phantom button ${String(h.id).split(':')[1] ?? h.id}`;
	return `Device ${h.id}`;
}

/** Scenes in the order of their phantom buttons. */
export const byButton = (scenes, status) => {
	const at = new Map((status?.scenes ?? []).map((s) => [s.device_id, s.button]));
	return [...scenes].sort(
		(x, y) =>
			(at.get(x.device_id) ?? 1000) - (at.get(y.device_id) ?? 1000) || x.name.localeCompare(y.name)
	);
};

/** Where the plugin's settings are, in the app. */
export const settingsHref = (hc, tab = 'settings') =>
	`/settings/plugins/${encodeURIComponent(hc.pluginId)}?tab=${tab}`;
