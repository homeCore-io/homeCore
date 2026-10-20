import { describe, expect, it } from 'vitest';
import { favouritesOf, groupName, groupsOf, groupSummary, nowLine, nowLines, positionNow, sleepLeft, sleepWords } from './sonos.js';

const sp = (id, name, a = {}) => ({ device_id: id, name, device_type: 'media_player', available: true, room: null, attributes: a });
const office1 = sp('sonos_office_1', 'Office-1', {
	state: 'paused',
	media_title: 'Kick It In the Sticks',
	media_artist: 'Brantley Gilbert',
	media_kind: 'track',
	media_position: 78,
	media_duration: 225,
	volume: 12,
	group_coordinator: 'sonos_office_1',
	group_members: ['sonos_office_1', 'sonos_office_2'],
	available_favorite_items: [{ title: 'ALT Radio', image_url: 'http://127.0.0.1:14001/art/alt-radio.png' }, { title: 'Piano for Sleeping' }]
});
const office2 = sp('sonos_office_2', 'Office-2', { ...office1.attributes, volume: 16 });
const bathroom = sp('sonos_bathroom', 'Bathroom', { state: 'stopped', media_title: 'ALT Radio', media_kind: 'radio', group_coordinator: 'sonos_bathroom' });
const living = sp('sonos_living_room', 'Living Room', { state: 'stopped', group_coordinator: 'sonos_office_9' });

describe('what a speaker plays, in words', () => {
	it('says a track, a station and nothing as the mockup does', () => {
		expect(nowLines(office1)).toEqual({ title: 'Kick It In the Sticks', sub: 'Brantley Gilbert · paused' });
		expect(nowLines(bathroom)).toEqual({ title: 'ALT Radio', sub: 'Radio · stopped' });
		expect(nowLines(living, { empty: 'Pick a favourite below' })).toMatchObject({ title: 'Nothing playing', sub: 'Pick a favourite below' });
		expect(nowLine(office1)).toBe('Kick It In the Sticks · paused');
		expect(nowLine(living)).toBe('Nothing playing');
	});
});

describe('groups', () => {
	it('makes one group of a leader and its members, leader first', () => {
		const gs = groupsOf([living, office2, bathroom, office1]);
		expect(gs.map((g) => g.name)).toEqual(['Bathroom', 'Living Room', 'Office-1 + Office-2']);
		const g = gs.find((x) => x.grouped);
		expect(g.leader.device_id).toBe('sonos_office_1');
		expect(g.members.map((m) => m.device_id)).toEqual(['sonos_office_1', 'sonos_office_2']);
	});

	it('puts a speaker whose leader is unknown on its own rather than losing it', () => {
		expect(groupsOf([living]).map((g) => g.id)).toEqual(['sonos_living_room']);
	});

	it('names a long group by its leader', () => {
		expect(groupName(['Office-1', 'Office-2', 'Bathroom'])).toBe('Office-1 + Office-2 + Bathroom');
		expect(groupName(['Office-1', 'Office-2', 'Bathroom', 'Living Room'])).toBe('Office-1 + 3 more');
	});

	it('says what happened, speaker by speaker', () => {
		const said = groupSummary([
			{ name: 'Office-2', result: 'joined' },
			{ name: 'Bathroom', result: 'joined' },
			{ name: 'Living Room', result: 'no_answer' }
		]);
		expect(said).toBe("Office-2 and Bathroom joined. Living Room didn't answer.");
		expect(groupSummary([{ name: 'Office-2', result: 'already' }])).toBe('Nothing needed changing.');
	});
});

describe('favourites and progress', () => {
	it('reads the favourites with their artwork from any speaker', () => {
		expect(favouritesOf([living, office1])).toEqual([
			{ title: 'ALT Radio', image: 'http://127.0.0.1:14001/art/alt-radio.png' },
			{ title: 'Piano for Sleeping', image: null }
		]);
		expect(favouritesOf([living])).toEqual([]);
	});

	it('moves a playing track on between reports, and never past its end', () => {
		const playing = { ...office1, attributes: { ...office1.attributes, state: 'playing' } };
		const seen = { pos: 78, at: 1000 };
		expect(positionNow(playing, seen, 11000).pos).toBe(88);
		expect(positionNow(office1, seen, 11000).pos).toBe(78);
		expect(positionNow(playing, seen, 1000 + 500_000).pos).toBe(225);
	});
});

describe('round two', () => {
	it('moves on from when the plugin says the position was true', () => {
		const at = Date.parse('2026-10-20T20:00:00.000Z');
		const playing = { ...office1, attributes: { ...office1.attributes, state: 'playing', media_position_updated_at: '2026-10-20T20:00:00.000Z' } };
		// A screen opened a minute after the last report still knows where it is.
		expect(positionNow(playing, null, at + 60_000).pos).toBe(138);
		// Paused, it stays put whatever the time.
		const paused = { ...playing, attributes: { ...playing.attributes, state: 'paused' } };
		expect(positionNow(paused, null, at + 60_000).pos).toBe(78);
	});

	it('says an input as an input', () => {
		const tv = sp('sonos_living_room', 'Living Room', { state: 'playing', source: 'TV', media_title: null });
		expect(nowLines(tv)).toEqual({ title: 'TV', sub: 'Input · playing' });
		expect(nowLine(tv)).toBe('TV · playing');
	});

	it('says how long the sleep timer has left', () => {
		const now = Date.parse('2026-10-20T22:00:00Z');
		const d = sp('x', 'X', { sleep_timer_ends_at: '2026-10-20T22:24:10Z' });
		expect(sleepLeft(d, now)).toBe(25);
		expect(sleepWords(25)).toBe('Stops in 25 min');
		expect(sleepWords(65)).toBe('Stops in 1 h 5 min');
		expect(sleepWords(60)).toBe('Stops in 1 h');
		expect(sleepLeft(sp('y', 'Y', { sleep_timer_ends_at: null }), now)).toBe(0);
	});
});
