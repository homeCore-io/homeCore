// The animated card's scenes: what each sky looks like, and the scatter of
// clouds, drops, flakes and stars it draws. Pure, so it can be tested.

/** The device's `condition` values and their names (conditions.rs). */
export const LABELS = {
	clear: 'Clear',
	partly_cloudy: 'Partly cloudy',
	cloudy: 'Cloudy',
	fog: 'Fog',
	drizzle: 'Drizzle',
	rain: 'Rain',
	freezing_rain: 'Freezing rain',
	snow: 'Snow',
	showers: 'Showers',
	thunderstorm: 'Thunderstorm'
};

/** Icon outlines on a 24 grid. */
export const ICONS = {
	sun: 'M12 4V2M12 22v-2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M6.3 17.7l-1.4 1.4M19.1 4.9l-1.4 1.4M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8z',
	moon: 'M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z',
	partly: 'M12 2v2M4.9 4.9l1.4 1.4M20 12h2M19.1 4.9l-1.4 1.4M15.9 12.7a4 4 0 0 0-5.9-4.4M13 22H7a5 5 0 1 1 4.9-6H13a3 3 0 0 1 0 6z',
	cloud: 'M17.5 19H9a7 7 0 1 1 6.7-9h1.8a4.5 4.5 0 1 1 0 9z',
	rain: 'M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 2.5 8.2M16 14v6M8 14v6M12 16v6',
	snow: 'M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 2.5 8.2M8 15h.01M8 19h.01M12 17h.01M12 21h.01M16 15h.01M16 19h.01',
	storm: 'M6 16.3A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 .5 9M13 11l-4 6h6l-4 6',
	fog: 'M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 2.5 8.2M16 17H7M17 21H9'
};

/**
 * Each scene: its backdrop by day and by night, its clouds' colours, its
 * icon, how many clouds it has at least, and whether its glass needs to be
 * dark (a pale sky).
 */
export const SCENES = {
	clear: {
		label: 'Clear', icon: 'sun', clouds: 0,
		day: 'linear-gradient(165deg, #2f80cf 0%, #63a8e6 55%, #a9d3f2 100%)',
		night: 'linear-gradient(170deg, #070d24 0%, #17215a 60%, #2c3478 100%)',
		cloudDay: ['#ffffff', '#ffffff'], cloudNight: ['#2c3445', '#3c4559']
	},
	partly: {
		label: 'Partly cloudy', icon: 'partly', clouds: 2,
		day: 'linear-gradient(165deg, #3c7fbd 0%, #79abd8 60%, #b7d3ea 100%)',
		night: 'linear-gradient(170deg, #0b1230 0%, #1f2a5e 65%, #3a4377 100%)',
		cloudDay: ['#ffffff', '#f4f7fb'], cloudNight: ['#2c3445', '#3c4559']
	},
	cloudy: {
		label: 'Cloudy', icon: 'cloud', clouds: 4,
		day: 'linear-gradient(170deg, #5f6b7b 0%, #8a95a3 60%, #b2bac3 100%)',
		night: 'linear-gradient(170deg, #1b2029 0%, #2e3540 65%, #444c58 100%)',
		cloudDay: ['#aab2bc', '#c9cfd6'], cloudNight: ['#2c3445', '#3c4559']
	},
	rain: {
		label: 'Rain', icon: 'rain', clouds: 3,
		day: 'linear-gradient(170deg, #33424f 0%, #556776 60%, #77899a 100%)',
		night: 'linear-gradient(170deg, #10161e 0%, #1f2a35 60%, #2f3c4a 100%)',
		cloudDay: ['#56646f', '#6d7d8c'], cloudNight: ['#26303a', '#33404c']
	},
	snow: {
		label: 'Snow', icon: 'snow', clouds: 3, lightGlass: true,
		day: 'linear-gradient(170deg, #5d7b98 0%, #87a1ba 55%, #b8cadb 100%)',
		night: 'linear-gradient(170deg, #1c2638 0%, #34435a 60%, #4f6079 100%)',
		cloudDay: ['#b9c8d7', '#a8b9ca'], cloudNight: ['#6c7c93', '#5b6b82']
	},
	storm: {
		label: 'Thunderstorm', icon: 'storm', clouds: 3,
		day: 'linear-gradient(170deg, #1d1b2e 0%, #383156 60%, #574a78 100%)',
		night: 'linear-gradient(170deg, #0b0a14 0%, #1d1930 60%, #2e2747 100%)',
		cloudDay: ['#2f2a45', '#4a4366'], cloudNight: ['#1f1b2f', '#2d2842']
	},
	fog: {
		label: 'Fog', icon: 'fog', clouds: 0, lightGlass: true,
		day: 'linear-gradient(175deg, #6c7378 0%, #8f9592 60%, #aeaca3 100%)',
		night: 'linear-gradient(175deg, #23272b 0%, #3a3e42 60%, #50534f 100%)',
		cloudDay: ['#ffffff', '#ffffff'], cloudNight: ['#3c4559', '#3c4559']
	}
};

/** A `condition` as one of the scenes. Unknown or none: partly cloudy, the plainest sky. */
export function sceneOf(condition) {
	switch (condition) {
		case 'clear':
			return 'clear';
		case 'cloudy':
			return 'cloudy';
		case 'fog':
			return 'fog';
		case 'drizzle':
		case 'rain':
		case 'freezing_rain':
		case 'showers':
			return 'rain';
		case 'snow':
			return 'snow';
		case 'thunderstorm':
			return 'storm';
		default:
			return 'partly';
	}
}

/**
 * The scene's moving parts, the same every time for the same weather, so
 * nothing jumps when a reading changes a little.
 *
 * - `cover`: cloud cover in %, which sets how many clouds (at least the
 *   scene's own);
 * - `wind`: m/s, which slants the rain and snow and speeds the clouds;
 * - `heavy` or `light`: how much rain or snow.
 */
export function scatter(scene, { heavy = false, light = false, wind = 0, cover } = {}) {
	let seed = 7;
	const rnd = () => {
		seed = (seed * 16807) % 2147483647;
		return (seed - 1) / 2147483646;
	};
	const r = (a, b) => Math.round((a + rnd() * (b - a)) * 100) / 100;

	const slant = Math.round(Math.min(28, Math.max(0, wind) * 2.2));
	// Calm: a cloud crosses in about a minute and a half; a gale, in twenty seconds.
	const pace = Math.max(0.28, 1 - Math.max(0, wind) / 18);
	const fromCover = typeof cover === 'number' ? Math.round(cover / 20) : 0;
	const nClouds = scene === 'clear' || scene === 'fog' ? 0 : Math.min(5, Math.max(SCENES[scene].clouds, fromCover));

	const stars = Array.from({ length: 26 }, () => ({ x: r(2, 98), y: r(2, 60), z: r(1, 2.6), d: -r(0, 3.4) }));
	const clouds = Array.from({ length: nClouds }, () => ({
		top: r(-8, 38),
		w: Math.round(r(150, 260)),
		dur: Math.round(r(55, 85) * pace),
		del: -Math.round(r(0, 80) * pace),
		op: r(0.6, 0.88)
	}));
	const wet = scene === 'rain' || scene === 'storm';
	const nDrops = heavy ? 70 : light ? 14 : 28;
	const drops = wet
		? Array.from({ length: nDrops }, () => ({
				x: r(0, 100),
				dur: r(heavy ? 0.45 : 0.7, heavy ? 0.7 : 1.1),
				del: -r(0, 2),
				op: r(0.35, 0.9),
				len: r(4, heavy ? 9 : 6)
			}))
		: [];
	const flakes =
		scene === 'snow'
			? Array.from({ length: heavy ? 70 : 32 }, () => ({
					x: r(0, 100),
					s: r(2, heavy ? 5 : 4),
					dur: r(heavy ? 3.5 : 5, heavy ? 6 : 9),
					del: -r(0, 9),
					op: r(0.5, 1)
				}))
			: [];
	const fog =
		scene === 'fog'
			? [
					{ top: 18, h: 26, dur: 9, del: 0 },
					{ top: 46, h: 30, dur: 12, del: -4 },
					{ top: 72, h: 26, dur: 10, del: -7 }
				]
			: [];
	return { slant: wet || scene === 'snow' ? slant : 0, stars, clouds, drops, flakes, fog };
}
