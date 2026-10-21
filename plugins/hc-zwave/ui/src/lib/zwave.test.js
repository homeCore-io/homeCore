import { describe, expect, it } from 'vitest';
import {
	attention,
	cardLine,
	compactLine,
	eventWords,
	gone,
	historyFrom,
	lastUsedBySlot,
	lowBattery,
	modelLine,
	nowWords,
	parseSse,
	powerDetail,
	powerWords,
	relockBody,
	relockOf,
	routeWords,
	slotLine
} from './zwave.js';

const lock = (a) => ({ device_id: 'zwave_46', device_type: 'lock', name: 'Lock - Living Room', attributes: a });

describe('words for the network', () => {
	it('says what a device is doing', () => {
		expect(nowWords(lock({ locked: false, open: true }))).toBe('Unlocked · door open');
		expect(nowWords(lock({ locked: true, open: false }))).toBe('Locked');
		expect(nowWords(lock({ jammed: true, locked: false }))).toBe('Jammed');
		expect(nowWords({ device_type: 'switch', attributes: { on: true, power_w: 2.7 } })).toBe('On · 2.7 W');
		expect(nowWords({ device_type: 'switch', attributes: { on: false, power_w: 0 } })).toBe('Off');
		expect(nowWords({ device_type: 'contact_sensor', attributes: { open: false } })).toBe('Closed');
		expect(nowWords({ device_type: 'motion_sensor', attributes: { motion: false } })).toBe('Clear');
	});

	it('says how it is powered', () => {
		expect(powerWords({ power: 'mains' })).toBe('Mains');
		expect(powerWords({ power: 'battery', battery_pct: 99, sleeps: true })).toBe('Battery 99% · sleeps');
		expect(powerWords({ power: 'flirs', battery_pct: 29 })).toBe('Battery 29%');
		expect(powerDetail({ power: 'battery', wake_up_secs: 43200 })).toBe('Battery · sleeps, wakes every 12 h');
		expect(powerDetail({ power: 'mains' })).toBe('Mains · always listening');
	});

	it('names the model and kind', () => {
		expect(modelLine({ manufacturer: 'Zooz', model: 'ZEN15' }, { device_type: 'switch', attributes: { power_w: 1 } })).toBe('Zooz ZEN15 · plug');
		expect(modelLine({ model: 'DB2' }, { device_type: 'lock', attributes: {} })).toBe('DB2 · lock');
	});

	it('lists only what needs someone', () => {
		const status = {
			connection: { state: 'connected' },
			nodes: [
				{ node_id: 1, device_id: 'zwave_1', is_controller: true },
				{ node_id: 46, device_id: 'zwave_46', status: 'alive', battery_pct: 29, signal: 'good' },
				{ node_id: 42, device_id: 'zwave_42', status: 'alive', signal: 'fair' },
				{ node_id: 40, device_id: 'zwave_40', status: 'alive', signal: 'good' },
				{ node_id: 44, device_id: 'zwave_44', status: 'dead', signal: 'poor' }
			]
		};
		const devices = [lock({ battery_pct: 29 }), { device_id: 'zwave_42', name: 'Outlet - MBR Door', attributes: {} }, { device_id: 'zwave_40', name: 'x', attributes: {} }, { device_id: 'zwave_44', name: 'Door', attributes: {} }, { device_id: 'zwave_23', name: 'Old', attributes: {} }];
		expect(attention(status, devices).map((x) => `${x.name}: ${x.what}`)).toEqual(['Lock - Living Room: battery 29%', 'Outlet - MBR Door: signal fair', 'Door: not answering']);
		expect(gone(status, devices).map((d) => d.device_id)).toEqual(['zwave_23']);
		expect(gone({ ...status, connection: { state: 'disconnected' } }, devices)).toEqual([]);
		expect(lowBattery({ battery_pct: 29 }, lock({}))).toBe(true);
		expect(lowBattery({ battery_pct: 29 }, { device_type: 'motion_sensor', attributes: {} })).toBe(false);
	});

	it('describes a route', () => {
		expect(routeWords({ repeaters: [] })).toBe('Straight to the controller');
		expect(routeWords({ repeaters: [12, 7] })).toBe('Through nodes 12 and 7');
		expect(routeWords(null)).toBe('Not known yet');
	});
});

describe('a lock', () => {
	const names = { 2: 'Cleaner' };
	it('says each event', () => {
		expect(eventWords({ action: 'unlocked', method: 'keypad', slot: 2 }, names)).toEqual({ what: 'Unlocked with code 2 · Cleaner', how: 'keypad' });
		expect(eventWords({ action: 'locked', method: 'auto' })).toEqual({ what: 'Locked', how: 'by itself' });
		expect(eventWords({ action: 'unlocked', method: 'manual' })).toEqual({ what: 'Unlocked', how: 'by hand' });
		expect(eventWords({ action: 'locked', method: 'rf' }, {}, 'Leave home')).toEqual({ what: 'Locked', how: 'homeCore · Leave home' });
		expect(eventWords({ action: 'wrong_code', method: 'keypad' }).what).toBe('Wrong code entered');
		expect(eventWords({ action: 'jammed' }).what).toBe('Jammed — the bolt didn’t move');
	});

	it('reads its history from state changes, newest first', () => {
		const rows = [
			{ event: { changed: ['locked'], current: { last_lock_event: { at: '2026-10-20T10:00:00Z', action: 'locked' } } } },
			{ event: { changed: ['last_lock_event'], change: { actor_name: 'Leave home' }, current: { last_lock_event: { at: '2026-10-20T08:30:00Z', action: 'locked', method: 'rf' } } } },
			{ event: { changed: ['last_lock_event'], current: { last_lock_event: { at: '2026-10-20T14:14:00Z', action: 'unlocked', method: 'keypad', slot: 2 } } } }
		];
		const h = historyFrom(rows, names);
		expect(h.map((x) => x.what)).toEqual(['Unlocked with code 2 · Cleaner', 'Locked']);
		expect(h[1].how).toBe('homeCore · Leave home');
		expect(lastUsedBySlot(h)).toEqual({ 2: '2026-10-20T14:14:00Z' });
	});

	it('says a slot', () => {
		const now = Date.parse('2026-10-20T18:00:00Z');
		expect(slotLine({ status: 'disabled' })).toBe('Turned off · kept on the lock');
		expect(slotLine({ status: 'in_use' })).toBe('In use');
		expect(slotLine({ status: 'in_use' }, '2026-10-19T12:00:00Z', now)).toBe('In use · last used yesterday');
		expect(slotLine({ status: 'available' })).toBe('');
	});

	it('locks again by itself the way the lock takes it', () => {
		expect(relockOf(lock({ lock_operation_type: 1, lock_timeout_secs: 30 }))).toBe(0);
		expect(relockOf(lock({ lock_operation_type: 2, lock_timeout_secs: 30 }))).toBe(30);
		expect(relockOf(lock({ lock_auto_relock_secs: 60 }))).toBe(60);
		expect(relockBody(lock({ lock_operation_type: 1 }), 30)).toEqual({ lock_operation_type: 2, lock_timeout_secs: 30 });
		expect(relockBody(lock({ lock_operation_type: 2 }), 0)).toEqual({ lock_operation_type: 1 });
		expect(relockBody(lock({ lock_auto_relock_secs: 0 }), 300)).toEqual({ lock_auto_relock_secs: 300 });
	});

	it('has card lines', () => {
		const d = lock({ locked: true, battery_pct: 29, last_lock_event: { action: 'locked', method: 'auto', at: '2026-10-20T13:02:00Z' } });
		expect(cardLine(d)).toMatch(/^by itself · .* · battery 29%$/);
		expect(compactLine(d)).toMatch(/^Locked · by itself /);
		expect(compactLine(lock({ locked: false }))).toBe('Unlocked');
	});
});

describe('streams', () => {
	it('reads an SSE block', () => {
		expect(parseSse('event: stream\ndata: {"stage":"progress","percent":40}')).toEqual([{ stage: 'progress', percent: 40 }]);
		expect(parseSse(': keep-alive')).toEqual([]);
	});
});
