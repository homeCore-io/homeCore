import { describe, expect, it } from 'vitest';
import {
	ago,
	band,
	cardLine,
	choices,
	diffText,
	holdChips,
	holdSaid,
	isSensor,
	isSwitch,
	kindOf,
	needsLine,
	needsYou,
	nextTarget,
	nowOf,
	pageLine,
	ranToday,
	readingsOf,
	shortName,
	span,
	targetText,
	targetsAround,
	tempText,
	todayLine
} from './thermo.js';
import { onRuns, pathOf, yRange } from './chart.js';

const F = { temperature: 'F' };
const C = { temperature: 'C' };
const NOW = Date.parse('2026-10-21T18:30:00Z');
const iso = (secsAgo) => new Date(NOW - secsAgo * 1000).toISOString();

// As hc-thermostat publishes them: the MBR AC cooling, the office one whose
// sensor went quiet, the garage heater idle.
const mbr = {
	device_id: 'thermostat_mbr',
	device_type: 'thermostat',
	available: true,
	name: 'Master Bedroom AC Thermostat',
	attributes: {
		kind: 'ac',
		temperature: 21.5,
		setpoint: 20,
		hysteresis: 1,
		hvac_mode: 'cool',
		hvac_action: 'cooling',
		reason: 'cooling',
		actuator_device_id: 'mbr_ac',
		actuator_state: true,
		actuator_confirmed: true,
		actuator_online: true,
		on_since: iso(600),
		run_secs_today: 3600,
		cycles_today: 3,
		sensor_ids: ['mbr_temp'],
		sensor_readings: { mbr_temp: { temperature: 21.5, at: iso(240), stale: false, online: true } },
		min_on_secs: 180,
		min_off_secs: 300
	}
};
const office = {
	...mbr,
	device_id: 'thermostat_office',
	attributes: {
		...mbr.attributes,
		temperature: null,
		hvac_action: 'idle',
		reason: 'sensors_stale',
		sensors_stale: true,
		actuator_state: false,
		sensor_ids: ['office_temp'],
		sensor_readings: {
			office_temp: { temperature: 21.2, at: iso(2 * 3600 + 14 * 60), stale: true, online: true }
		}
	}
};
const names = {
	mbr_temp: 'MBR Temperature',
	office_temp: 'Office Desk Temperature',
	mbr_ac: 'Outlet - MBR AC'
};

describe('temperatures', () => {
	it('reads in the person’s unit, targets whole °F or half °C', () => {
		expect(tempText(21.5, F)).toBe('70.7°');
		expect(tempText(21.5, C)).toBe('21.5°');
		expect(tempText(null, F)).toBe('—');
		expect(targetText(20, F)).toBe('68°');
		expect(targetText(20.4, C)).toBe('20.5°');
		expect(diffText(0.5, F)).toBe('0.9°');
	});

	it('steps half a degree Celsius, never stopping on a step that reads the same', () => {
		expect(nextTarget(20, 1, C)).toBe(20.5);
		expect(nextTarget(20, 1, F)).toBe(20.5); // 68 → 69
		// 22.5 °C reads 73 °F, and so does 23: one press goes past both.
		expect(targetText(22.5, F)).toBe(targetText(23, F));
		expect(nextTarget(22.5, 1, F)).toBe(23.5);
		expect(nextTarget(45, 1, C)).toBe(45);
	});

	it('offers targets that each read differently', () => {
		const ts = targetsAround(20, F, 4, 4).map((t) => t.text);
		expect(new Set(ts).size).toBe(ts.length);
		expect(ts).toContain('68°');
	});

	it('knows the band either side of the target', () => {
		expect(band(mbr)).toEqual({ target: 20, half: 0.5, low: 19.5, high: 20.5 });
	});
});

describe('time', () => {
	it('says spans and ages plainly', () => {
		expect(span(3 * 3600 + 12 * 60)).toBe('3 h 12 m');
		expect(span(108)).toBe('1 m 48 s');
		expect(span(52 * 60)).toBe('52 m');
		expect(ago(iso(240), NOW)).toBe('4 min ago');
		expect(ago(iso(20), NOW)).toBe('just now');
		expect(ago(iso(194 * 3600), NOW)).toBe('8 days ago');
	});

	it('counts a run going on now into today', () => {
		expect(ranToday(mbr, NOW)).toBe(3600 + 600);
		expect(todayLine(mbr, NOW)).toBe('ran 1 h 10 m · 3 times');
		expect(todayLine({ attributes: { cycles_today: 0, run_secs_today: 0 } }, NOW)).toBe(
			"hasn't run today"
		);
	});
});

describe('what it is doing', () => {
	it('says why it is cooling, in °F', () => {
		const n = nowOf(mbr, { units: F, names, now: NOW });
		expect(n.title).toBe('Cooling');
		expect(n.why).toBe(
			'The room is 70.7°, above 68.9° (the target plus the swing). It stops at 67.1°.'
		);
		expect(cardLine(mbr, { units: F })).toBe('Cooling to 68°');
		expect(pageLine(mbr, { units: F })).toBe('Cooling — 70.7° is above 68.9°');
	});

	it('says a quiet sensor, its name, and what was done', () => {
		const n = nowOf(office, { units: F, names, now: NOW });
		expect(n.title).toBe('Off — no reading for 2 h 14 m');
		expect(n.why).toMatch(
			/^Office Desk Temperature last reported at .+\. With no current reading the AC was turned off/
		);
		expect(n.act.kind).toBe('alert');
		expect(needsYou(office)).toBe(true);
		expect(needsYou(mbr)).toBe(false);
		expect(cardLine(office, { units: F, now: NOW })).toBe('Off — sensor quiet 2 h');
		expect(needsLine(office, { units: F, names, now: NOW })).toMatch(
			/^Office Desk Temperature hasn't reported since .+, so the AC was turned off\.$/
		);
		expect(pageLine(office, { units: F, now: NOW })).toBe(
			'Off — its sensor has been quiet for 2 h 14 m'
		);
	});

	it('words each reason, with the button that helps', () => {
		const w = (attrs) =>
			nowOf({ ...mbr, attributes: { ...mbr.attributes, ...attrs } }, { units: F, names, now: NOW });
		expect(
			w({ reason: 'waiting_min_off', lockout_until: iso(-192), actuator_last_change: iso(108) })
		).toMatchObject({
			title: 'Waiting 3:12',
			act: { kind: 'start_now' }
		});
		expect(
			w({ reason: 'waiting_min_off', lockout_until: iso(-192), actuator_last_change: iso(108) }).why
		).toContain('went off 1 m 48 s ago and stays off at least 5 min to protect the compressor');
		expect(
			w({ reason: 'override', override_until: iso(-3600), actuator_state: false }).act.kind
		).toBe('resume');
		expect(w({ reason: 'actuator_not_responding', actuator_online: false }).title).toBe(
			'Outlet - MBR AC is offline'
		);
		expect(w({ reason: 'off', hvac_mode: 'off' }).why).toContain('leaves the AC alone');
		expect(w({ reason: 'idle_in_band', temperature: 20.1 }).why).toBe(
			'The room is 68.2°, within 68° ± 0.9°.'
		);
		expect(w({ reason: 'idle_in_band', temperature: 18 }).why).toContain('nothing to cool');
	});

	it('takes the kind from the mode for a thermostat from before kinds', () => {
		expect(kindOf({ attributes: { hvac_mode: 'heat' } })).toBe('heater');
		expect(kindOf({ attributes: { hvac_mode: 'cool' } })).toBe('ac');
		expect(kindOf({ attributes: { kind: 'fan', hvac_mode: 'cool' } })).toBe('fan');
	});

	it('reads its sensors', () => {
		expect(readingsOf(office)).toEqual([
			{ id: 'office_temp', temperature: 21.2, at: iso(8040), stale: true, online: true }
		]);
	});
});

describe('holds', () => {
	it('offers a degree lower, and higher, in the person’s unit', () => {
		const c = holdChips(mbr, F, NOW).map((x) => x.name);
		expect(c.slice(0, 3)).toEqual(['66° for 1 h', '66° for 2 h', '70° for 4 h']);
		expect(holdChips(mbr, F, NOW)[0].setpoint).toBe(19);
	});

	it('says a hold and where it goes back to', () => {
		expect(holdSaid(mbr, F)).toMatch(/^For a nap/);
		expect(
			holdSaid(
				{ ...mbr, attributes: { ...mbr.attributes, hold_setpoint: 19, hold_until: iso(-3600) } },
				F
			)
		).toMatch(/^Holding 66° until .+ — then back to 68°\.$/);
	});
});

describe('the house', () => {
	const house = [
		{
			device_id: 'zwave_38',
			name: 'Outlet - Office AC',
			device_type: 'switch',
			plugin_id: 'plugin.zwave',
			area: 'office',
			attributes: { on: false }
		},
		{
			device_id: 'zwave_39',
			name: 'Outlet - MBR AC',
			device_type: 'switch',
			plugin_id: 'plugin.zwave',
			area_override: 'master_bedroom',
			attributes: { on: true }
		},
		{
			device_id: 'hue_1',
			name: 'Desk Lamp',
			device_type: 'light',
			plugin_id: 'plugin.hue',
			attributes: { on: true, brightness_pct: 40 }
		},
		{
			device_id: 'yl_lock',
			name: 'Front Door',
			device_type: 'lock',
			plugin_id: 'plugin.yolink',
			attributes: { on: false, locked: true }
		},
		{
			device_id: 'thermostat_mbr',
			name: 'MBR',
			device_type: 'thermostat',
			plugin_id: 'plugin.thermostat',
			attributes: { temperature: 21 }
		},
		{
			device_id: 'core_sw',
			name: 'Guest mode',
			device_type: 'switch',
			plugin_id: 'core.switch',
			attributes: { on: false }
		},
		{
			device_id: 'yl_temp',
			name: 'Office Desk Temperature',
			device_type: 'temperature_sensor',
			plugin_id: 'plugin.yolink',
			area: 'office',
			attributes: { temperature: 23 }
		}
	];
	const rooms = [
		{ id: 'r1', name: 'Office', slug: 'office' },
		{ id: 'r2', name: 'Master Bedroom', slug: 'master_bedroom' }
	];

	it('offers plugs and switches only — not lights, locks, thermostats or flags', () => {
		expect(house.filter(isSwitch).map((d) => d.device_id)).toEqual(['zwave_38', 'zwave_39']);
		expect(house.filter(isSensor).map((d) => d.device_id)).toEqual(['yl_temp']);
	});

	it('marks a plug already run by a thermostat, with rooms', () => {
		const c = choices(
			house,
			rooms,
			[{ ...mbr, attributes: { ...mbr.attributes, actuator_device_id: 'zwave_39' } }],
			(d) => d.name
		);
		expect(c.plugs).toEqual([
			expect.objectContaining({
				id: 'zwave_39',
				room: 'Master Bedroom',
				usedBy: 'Master Bedroom AC Thermostat'
			}),
			expect.objectContaining({ id: 'zwave_38', room: 'Office', usedBy: null })
		]);
		expect(c.sensors[0]).toMatchObject({ id: 'yl_temp', room: 'Office', temperature: 23 });
	});

	it('shortens a card’s name', () => {
		expect(shortName('Master Bedroom AC Thermostat')).toBe('Master Bedroom AC');
		expect(shortName('Den')).toBe('Den');
	});
});

describe('the chart', () => {
	const start = Date.parse('2026-10-21T04:00:00Z');
	const at = (h) => start + h * 3600e3;
	const row = (h, v) => ({ at: new Date(at(h)).toISOString(), value: v });

	it('shades the runs the history shows', () => {
		expect(onRuns([row(1, true), row(2, false), row(5, true)], start, at(6))).toEqual([
			{ from: at(1), to: at(2) },
			{ from: at(5), to: at(6) }
		]);
	});

	it('shades nothing it can’t know — unless on_since says the run began earlier', () => {
		expect(onRuns([row(3, false)], start, at(6))).toEqual([]);
		expect(onRuns([], start, at(6), at(-2))).toEqual([{ from: start, to: at(6) }]);
		expect(onRuns([row(5, true)], start, at(6), at(4.5))).toEqual([{ from: at(4.5), to: at(6) }]);
	});

	it('fits the band and the readings, and draws steps', () => {
		const y = yRange([{ v: 22 }], { low: 19.5, high: 20.5 });
		expect(y.min).toBeLessThan(19.5);
		expect(y.max).toBeGreaterThan(22);
		expect(
			pathOf(
				[
					{ at: 0, v: 1 },
					{ at: 10, v: 2 }
				],
				(t) => t,
				(v) => v
			)
		).toBe('M0.0,1.0H10.0V2.0');
	});
});
