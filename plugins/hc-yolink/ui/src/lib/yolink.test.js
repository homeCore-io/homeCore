import { describe, expect, it } from 'vitest';
import {
	batteries,
	batteryShort,
	becameTrue,
	cardLine,
	historyFrom,
	hubLine,
	lastLine,
	lastProblem,
	lastWet,
	leakAutomations,
	leakLine,
	lockSubline,
	quarters,
	switchWords,
	temp,
	toCelsius,
	water
} from './yolink.js';

const NOW = Date.parse('2026-10-21T18:00:00Z');
const dev = (device_type, attributes, extra = {}) => ({ device_id: `yolink_${Math.random()}`, name: 'X', device_type, available: true, attributes, ...extra });
const host = (f) => ({ units: { show: (v, u) => (f && u === '°C' ? { value: Math.round((v * 9) / 5 + 32), unit: '°F', text: `${Math.round((v * 9) / 5 + 32)} °F` } : { value: v, unit: u, text: `${v} ${u}` }), toCore: (v, u) => (f && u === '°C' ? ((v - 32) * 5) / 9 : v) } });

describe('batteries in quarters', () => {
	it('reads YoLink’s levels back from battery_pct', () => {
		expect(quarters(dev('lock', { battery_pct: 25 }))).toBe(1);
		expect(batteryShort(dev('lock', { battery_pct: 100 }))).toBe('full');
		expect(batteryShort(dev('lock', { battery_pct: 50 }))).toBe('2 of 4');
	});
	it('lists the less-than-full, lowest first', () => {
		const list = batteries([dev('lock', { battery_pct: 75 }, { name: 'B' }), dev('lock', { battery_pct: 25 }, { name: 'A' }), dev('lock', { battery_pct: 100 }, { name: 'C' })]);
		expect(list.map((d) => d.name)).toEqual(['A', 'B']);
	});
});

describe('switches and locks in words', () => {
	it('says a metered plug’s draw, and nothing for one without', () => {
		expect(switchWords(dev('switch', { on: true, power_w: 3.7 }))).toBe('On · 3.7 W');
		expect(switchWords(dev('switch', { on: false }))).toBe('Off');
	});
	it('says how a lock last moved', () => {
		const d = dev('lock', { locked: false, last_lock_event: { action: 'unlocked', method: 'fingerprint', at: '2026-10-21T17:05:00Z' } });
		expect(lastLine(d, NOW)).toMatch(/^Unlocked by fingerprint · /);
		const auto = dev('lock', { locked: true, last_lock_event: { action: 'locked', method: 'auto', at: '2026-10-21T14:14:00Z' } });
		expect(cardLine(auto, NOW)).toMatch(/^By itself · /);
		expect(lockSubline(dev('lock', { auto_lock_secs: 10 }))).toBe('Locks itself after 10 s');
		expect(lockSubline(dev('lock', { open: false, battery_pct: 25, battery_low: true }))).toBe('Door closed · battery low');
	});
	it('says the last problem, with when and from where', () => {
		const p = lastProblem(dev('lock', { last_alert: 'UnLockFailed', last_alert_source: 'app', last_alert_at: '2026-10-18T12:00:00Z' }), NOW);
		expect(p.what).toBe('Couldn’t unlock');
		expect(p.line).toMatch(/from the app$/);
		expect(lastProblem(dev('lock', { last_alert: 'LockFailed' }), NOW).line).toBe('when, YoLink didn’t say');
		expect(lastProblem(dev('lock', {}), NOW)).toBe(null);
	});
	it('builds the history from lock events and the door, newest first, naming the automation', () => {
		const h = historyFrom({
			lockEvents: [
				{ action: 'unlock_failed', method: 'app', at: '2026-10-18T10:00:00Z' },
				{ action: 'unlocked', method: 'homecore', at: '2026-10-20T10:00:00Z' }
			],
			doorOpened: ['2026-10-20T10:01:00Z'],
			log: [{ event: { current: { last_lock_event: { at: '2026-10-20T10:00:00Z' } }, change: { actor_name: 'Morning' } } }]
		});
		expect(h.map((x) => x.what)).toEqual(['Door opened', 'Unlocked', 'Couldn’t unlock']);
		expect(h[1].how).toBe('homeCore · Morning');
		expect(h[2].bad).toBe(true);
	});
});

describe('water', () => {
	it('is never dry while not reporting', () => {
		const quiet = dev('water_sensor', { water_detected: false, last_seen: '2026-10-21T09:00:00Z' }, { available: false });
		expect(leakLine(quiet, NOW)).toBe('Not reporting · last 9 h ago');
		expect(leakLine(dev('water_sensor', { water_detected: false, last_seen: '2026-10-21T17:48:00Z' }), NOW)).toBe('Dry · checked in 12 min ago');
		const w = water([quiet, dev('water_sensor', { water_detected: true, last_seen: '2026-10-21T17:59:00Z' })], NOW);
		expect([w.total, w.wet.length, w.quiet.length, w.checkedIn.length]).toEqual([2, 1, 1, 1]);
	});
	it('finds the automations that start on a leak sensor getting wet', () => {
		const rules = [
			{ name: 'Water Valve Off', enabled: true, trigger: { DeviceStateChanged: { device_ids: ['yolink_a'], attribute: 'water_detected', to: true } } },
			{ name: 'Off', enabled: false, trigger: { DeviceStateChanged: { device_id: 'yolink_a', attribute: 'water_detected' } } },
			{ name: 'Other', enabled: true, trigger: { DeviceStateChanged: { device_id: 'yolink_b', attribute: 'open' } } }
		];
		expect(leakAutomations(rules, ['yolink_a']).map((r) => r.name)).toEqual(['Water Valve Off']);
	});
	it('finds the last wet spell and the door opening', () => {
		const pts = [
			{ at: '1', value: false },
			{ at: '2', value: true },
			{ at: '3', value: false },
			{ at: '4', value: true }
		];
		expect(lastWet(pts)).toEqual({ from: '4', to: null });
		expect(lastWet(pts.slice(0, 3))).toEqual({ from: '2', to: '3' });
		expect(becameTrue(pts)).toEqual(['2', '4']);
	});
});

describe('units', () => {
	it('shows °C as the person reads it, and takes it back', () => {
		expect(temp(host(false), -14.3)).toBe('−14.3 °C');
		expect(temp(host(true), -10)).toBe('14 °F');
		expect(temp(undefined, 4)).toBe('4 °C');
		expect(toCelsius(host(true), 14)).toBe(-10);
	});
	it('says where the hub is and how the feed is', () => {
		const s = { configured: true, mode: 'local', hub: { model: 'YS1606-UC' }, feed: { connected: true }, last_event_at: '2026-10-21T17:58:00Z' };
		expect(hubLine(s, NOW)).toBe('Local hub YS1606 · live events · last event 2 min ago');
		expect(hubLine({ ...s, feed: { connected: false } }, NOW)).toMatch(/live events are down/);
	});
});
