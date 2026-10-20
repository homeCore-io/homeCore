// What the Sonos screens share: speakers and their groups, what each is
// playing in words, the favourites, and the commands — all from the plugin's
// published state, through the `host` each screen is handed.

/** Give a shadow root the app's parts (host.theme.sheet) beside its own. */
export function adopt(el, host) {
	const root = el?.shadowRoot;
	if (!root || !host) return;
	const sheet = host.theme.sheet;
	if (!root.adoptedStyleSheets.includes(sheet)) root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
}

export const num = (v) => {
	const n = typeof v === 'string' && v.trim() !== '' ? Number(v) : v;
	return typeof n === 'number' && Number.isFinite(n) ? n : undefined;
};

const attrs = (d) => d?.attributes ?? {};

/** The plugin's speakers, by name. */
export function speakersOf(devices, label = (d) => d.name) {
	return devices.filter((d) => d.device_type === 'media_player').sort((a, b) => label(a).localeCompare(label(b)));
}

/** "playing", "paused", "stopped", "starting". */
export function playbackWord(d) {
	const s = attrs(d).state;
	return s === 'buffering' ? 'starting' : typeof s === 'string' ? s : 'stopped';
}
export const isPlaying = (d) => attrs(d).state === 'playing' || attrs(d).state === 'buffering';
export const isRadio = (d) => attrs(d).media_kind === 'radio';
/** An input playing — `TV`, `Line-in`, as the plugin says it; '' for none. */
export const inputOf = (d) => (typeof attrs(d).source === 'string' ? attrs(d).source : '');
export const hasMusic = (d) => (typeof attrs(d).media_title === 'string' && attrs(d).media_title !== '') || inputOf(d) !== '';
export const art = (d) => (typeof attrs(d).media_image_url === 'string' && attrs(d).media_image_url) || null;
export const volumeOf = (d) => num(attrs(d).volume) ?? 0;
export const groupVolumeOf = (d) => num(attrs(d).group_volume);

/** What it plays, in two lines: the title, then who and how it is. */
export function nowLines(d, { empty = 'Pick a favourite' } = {}) {
	const a = attrs(d);
	if (!hasMusic(d)) return { title: 'Nothing playing', sub: empty, nothing: true };
	const how = playbackWord(d);
	if (inputOf(d)) return { title: inputOf(d), sub: `Input · ${how}` };
	if (isRadio(d)) return { title: a.media_title, sub: `Radio · ${how}` };
	return { title: a.media_title, sub: [a.media_artist, how].filter(Boolean).join(' · ') };
}

/** One line: "Kick It In the Sticks · paused", "Nothing playing". */
export function nowLine(d, sep = ' · ') {
	if (!hasMusic(d)) return 'Nothing playing';
	return `${inputOf(d) || attrs(d).media_title}${sep}${playbackWord(d)}`;
}

/** The favourites, from whichever speaker has the catalogue: `[{title, image}]`. */
export function favouritesOf(devices) {
	for (const d of devices) {
		const items = attrs(d).available_favorite_items;
		if (Array.isArray(items) && items.length)
			return items
				.filter((f) => typeof f?.title === 'string' && f.title)
				.map((f) => ({ title: f.title, image: f.image_url ?? f.albumArtUri ?? null }));
	}
	for (const d of devices) {
		const names = attrs(d).available_favorites;
		if (Array.isArray(names) && names.length) return names.map((title) => ({ title, image: null }));
	}
	return [];
}

/**
 * The speakers as groups: the leader first, then its members by name. A
 * speaker alone is a group of one. A speaker whose leader is not among them
 * is put on its own, rather than lost.
 */
export function groupsOf(devices, label = (d) => d.name) {
	const list = speakersOf(devices, label);
	const ids = new Set(list.map((d) => d.device_id));
	const leaderOf = (d) => {
		const c = attrs(d).group_coordinator;
		return typeof c === 'string' && ids.has(c) ? c : d.device_id;
	};
	const by = new Map();
	for (const d of list) {
		const l = leaderOf(d);
		if (!by.has(l)) by.set(l, []);
		by.get(l).push(d);
	}
	return [...by.entries()]
		.map(([leaderId, members]) => {
			const leader = members.find((m) => m.device_id === leaderId) ?? members[0];
			const rest = members.filter((m) => m !== leader).sort((a, b) => label(a).localeCompare(label(b)));
			const all = [leader, ...rest];
			return { id: leader.device_id, leader, members: all, grouped: all.length > 1, name: groupName(all.map(label)) };
		})
		.sort((a, b) => label(a.leader).localeCompare(label(b.leader)));
}

/** "Office-1 + Office-2"; a longer group "Office-1 + 2 more". */
export function groupName(names) {
	if (names.length <= 1) return names[0] ?? '';
	const joined = names.join(' + ');
	return joined.length <= 36 ? joined : `${names[0]} + ${names.length - 1} more`;
}

/** The ids in `d`'s group, leader first, or just its own. */
export function groupIdsOf(d) {
	const m = attrs(d).group_members;
	return Array.isArray(m) && m.length ? m : [d.device_id];
}
export const leaderIdOf = (d) => (typeof attrs(d).group_coordinator === 'string' ? attrs(d).group_coordinator : d.device_id);
export const isMember = (d) => leaderIdOf(d) !== d.device_id;

/** "A", "A and B", "A, B and C". */
export function listWords(items) {
	if (items.length <= 1) return items.join('');
	return `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`;
}

/** 3:45 from 225. */
export function clock(secs) {
	const s = Math.max(0, Math.floor(num(secs) ?? 0));
	const h = Math.floor(s / 3600);
	const m = Math.floor(s / 60) % 60;
	const ss = String(s % 60).padStart(2, '0');
	return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

/** "just now", "3 min ago", "2 h ago", from an ISO time. */
export function ago(iso, now = Date.now()) {
	const t = Date.parse(iso ?? '');
	if (!Number.isFinite(t)) return '';
	const s = Math.max(0, (now - t) / 1000);
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s < 86400) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/**
 * Where a track is now: the last reported position, moved on by the time
 * since it was true while it plays. The plugin says when
 * (`media_position_updated_at`) and only publishes the position again when it
 * jumps; without that, `seen` (`{ pos, at }`, when this screen saw it) stands in.
 */
export function positionNow(d, seen, now = Date.now()) {
	const a = attrs(d);
	const pos = num(a.media_position) ?? 0;
	const dur = num(a.media_duration) ?? 0;
	const at = Date.parse(a.media_position_updated_at ?? '');
	let p = pos;
	if (isPlaying(d)) {
		if (Number.isFinite(at)) p = pos + Math.max(0, now - at) / 1000;
		else if (seen && seen.pos === pos) p = pos + (now - seen.at) / 1000;
	}
	return { pos: dur ? Math.min(p, dur) : p, dur };
}

/** Minutes left on the sleep timer, rounded up; 0 with none. */
export function sleepLeft(d, now = Date.now()) {
	const end = Date.parse(attrs(d).sleep_timer_ends_at ?? '');
	if (!Number.isFinite(end) || end <= now) return 0;
	return Math.ceil((end - now) / 60000);
}

/** "Stops in 25 min", "Stops in 1 h 5 min". */
export function sleepWords(mins) {
	if (!mins) return '';
	const h = Math.floor(mins / 60);
	const m = mins % 60;
	return `Stops in ${h ? `${h} h${m ? ` ${m} min` : ''}` : `${m} min`}`;
}

/** The sleep timer's choices, in minutes; 0 is off. */
export const SLEEP_CHOICES = [0, 15, 30, 45, 60, 90, 120];

/** Words for one speaker's answer to a `group`. */
export const RESULT_WORDS = { joined: 'joined', left: 'left', already: 'in the group', no_answer: 'no answer', failed: 'said no' };

/** "Office-2 and Bathroom joined. Living Room didn't answer." */
export function groupSummary(results = []) {
	const names = (r) => listWords(results.filter((x) => x.result === r).map((x) => x.name));
	const parts = [];
	if (names('joined')) parts.push(`${names('joined')} joined.`);
	if (names('left')) parts.push(`${names('left')} left the group.`);
	if (names('no_answer')) parts.push(`${names('no_answer')} didn't answer.`);
	if (names('failed')) parts.push(`${names('failed')} said no.`);
	return parts.join(' ') || 'Nothing needed changing.';
}

/** Ask the plugin to make `leader`'s group exactly it and `members`. */
export async function makeGroup(hc, leader, members) {
	const r = await hc.action('group_speakers', { leader, members });
	if (!r.ok) return { ok: false, error: r.error, results: [] };
	return { ok: true, results: Array.isArray(r.data.results) ? r.data.results : [], data: r.data };
}

/** Play or pause; play when nothing is going. */
export const playPause = (hc, d) => hc.command(d.device_id, { action: isPlaying(d) ? 'pause' : 'play' });

/**
 * A slider's sends, no faster than every `ms`, the last one always sent:
 * a drag sends as it goes, and lands where it was let go.
 */
export function throttle(f, ms = 250) {
	let last = 0;
	let timer = null;
	let pending = null;
	return (...args) => {
		pending = args;
		const wait = last + ms - Date.now();
		if (wait <= 0) {
			last = Date.now();
			f(...pending);
			pending = null;
		} else if (!timer) {
			timer = setTimeout(() => {
				timer = null;
				last = Date.now();
				if (pending) f(...pending);
				pending = null;
			}, wait);
		}
	};
}

/** Where the plugin's own page and settings are, in the app. */
export const pageHref = (hc) => `/settings/plugins/${encodeURIComponent(hc.pluginId)}`;
export const settingsHref = (hc) => `${pageHref(hc)}?tab=settings`;
export const devicesHref = (hc) => `${pageHref(hc)}?tab=devices`;
