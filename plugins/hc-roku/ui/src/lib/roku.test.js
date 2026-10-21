import { describe, expect, it } from 'vitest';
import {
	appsOf,
	az,
	clock,
	iconUrl,
	inputsOf,
	mostUsed,
	nowOf,
	positionNow,
	search,
	short,
	stateLine
} from './roku.js';

// As hc-roku publishes them (house shapes: the soundbar on YouTube, the
// Office TV on HDMI 1, the 65" in standby).
const apps = [
	{ id: '12', name: 'Netflix', icon: 'icons/12-5.1.png', launches: 2 },
	{ id: '13', name: 'Prime Video', icon: null, launches: 0 },
	{ id: '837', name: 'YouTube', icon: 'icons/837-2.23.png', launches: 9 },
	{ id: '13535', name: 'Plex - Free Movies & TV', icon: null, launches: 2 }
];
const bar = {
	device_id: 'roku_yl0001073725',
	available: true,
	device_type: 'media_player',
	attributes: {
		on: true,
		state: 'playing',
		app_id: '837',
		app_name: 'YouTube',
		is_tv_input: false,
		media_position: 1210,
		media_duration: 3760,
		media_position_updated_at: '2026-10-21T20:00:00.000Z',
		available_apps: apps,
		available_inputs: []
	}
};
const office = {
	device_id: 'roku_yk00xf435811',
	available: true,
	attributes: {
		on: true,
		state: 'idle',
		app_id: 'tvinput.hdmi1',
		is_tv_input: true,
		available_apps: apps,
		available_inputs: [{ id: 'tvinput.hdmi1', name: 'HDMI 1' }]
	}
};
const tv65 = {
	device_id: 'roku_x00300cr4tpa',
	available: true,
	attributes: { on: false, available_apps: apps }
};

describe('channels', () => {
	it('puts the most used first, then the Roku’s own order', () => {
		expect(mostUsed(bar, 3).map((a) => a.id)).toEqual(['837', '12', '13535']);
	});
	it('finds one A–Z, ignoring case', () => {
		expect(az(appsOf(bar)).map((a) => a.name)[0]).toBe('Netflix');
		expect(search(appsOf(bar), 'plex').map((a) => a.id)).toEqual(['13535']);
	});
	it('shortens a long name for a tile', () => {
		expect(short('Plex - Free Movies & TV')).toBe('Plex');
		expect(short('Roku | Cinematic Sound')).toBe('Cinematic Sound');
	});
	it('makes an icon’s address only when the plugin kept one and the app can say where', () => {
		const hc = { fileUrl: (p) => `/files/${p}` };
		expect(iconUrl(hc, apps[0])).toBe('/files/icons/12-5.1.png');
		expect(iconUrl(hc, apps[1])).toBeNull();
		expect(iconUrl({}, apps[0])).toBeNull();
	});
	it('reads inputs by their own names', () => {
		expect(inputsOf(office).map((i) => i.name)).toEqual(['HDMI 1']);
	});
});

describe('what it is doing', () => {
	it('says an app, an input, off and offline', () => {
		expect(nowOf(bar)).toMatchObject({ kind: 'app', name: 'YouTube', playing: true });
		expect(nowOf(office)).toMatchObject({ kind: 'input', name: 'HDMI 1' });
		expect(nowOf(tv65).kind).toBe('off');
		expect(nowOf({ ...bar, available: false }).kind).toBe('offline');
	});
	it('counts the position on from when it was true', () => {
		const at = Date.parse('2026-10-21T20:00:10.000Z');
		expect(positionNow(bar, at)).toEqual({ pos: 1220, dur: 3760 });
		expect(positionNow(office, at)).toBeNull();
	});
	it('says it in one line', () => {
		const at = Date.parse('2026-10-21T20:00:00.000Z');
		expect(stateLine(bar, at)).toBe('Playing YouTube · 20:10');
		expect(stateLine(office)).toBe('On · HDMI 1');
		expect(stateLine(tv65)).toBe('Off');
		expect(clock(3760)).toBe('1:02:40');
	});
});
