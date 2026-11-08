// A stand-in Reolink for the sandbox, answering /cgi-bin/api.cgi in Reolink's
// shapes. By default an NVR with two cameras: channel 0 a doorbell (person,
// package, visitor), channel 1 a PTZ camera with spotlight, siren, infrared
// and presets (person, vehicle, animal). MODE=camera makes it a camera on its
// own (type IPC, no channel list): one PTZ camera, or LENSES=2 a two-lens one
// (a Duo). Sessions are limited, as a real device's are, and refused sign-ins
// lock it for a while.
//
//   POST /standin/set  {"channel":1,"md":1,"people":1,...}  what a camera sees
//   GET  /standin/log  the commands it was sent
//   GET  /standin/state
import http from 'node:http';
import { readFileSync } from 'node:fs';
import { randomBytes } from 'node:crypto';

const PORT = Number(process.env.PORT ?? 18996);
const CAMERA = process.env.MODE === 'camera';
const LENSES = Number(process.env.LENSES ?? 1);
const USER = 'admin';
const PASS = process.env.PASS ?? 'standin-pass';
const MAX_SESSIONS = 4;
const JPEG = readFileSync(new URL('./snap.jpg', import.meta.url));

const sessions = new Map(); // token -> expires
let refused = 0;
let lockedUntil = 0;
const log = [];
const chans = [
	{ name: 'Front door', type: 'Reolink Video Doorbell', online: 1, ev: { md: 0, people: 0, vehicle: 0, dog_cat: 0, package: 0, visitor: 0 },
		support: { people: 1, vehicle: 0, dog_cat: 0, package: 1, visitor: 1 }, led: null, ir: 'Auto', ptz: false },
	{ name: 'Driveway', type: 'RLC-823A', online: 1, ev: { md: 0, people: 0, vehicle: 0, dog_cat: 0, package: 0, visitor: 0 },
		support: { people: 1, vehicle: 1, dog_cat: 1, package: 0, visitor: 0 }, led: { state: 0, bright: 80 }, ir: 'Auto', ptz: true }
];
// A camera on its own is the PTZ camera, once per lens.
if (CAMERA) {
	const cam = chans[1];
	chans.length = 0;
	for (let i = 0; i < LENSES; i++) chans.push({ ...structuredClone(cam), name: 'Porch' });
}
const presets = [{ enable: 1, id: 1, name: 'Gate' }, { enable: 1, id: 2, name: 'Garage' }, { enable: 0, id: 3, name: '' }];

const ok = (cmd, value) => ({ cmd, code: 0, value });
const err = (cmd, rspCode, detail) => ({ cmd, code: 1, error: { rspCode, detail } });
const ver = (on) => ({ permit: on ? 6 : 0, ver: on ? 1 : 0 });

function answer(c, token) {
	const p = c.param ?? {};
	const ch = chans[p.channel ?? p.WhiteLed?.channel ?? p.IrLights?.channel ?? 0];
	switch (c.cmd) {
		case 'GetDevInfo':
			return CAMERA
				? ok(c.cmd, { DevInfo: { model: LENSES > 1 ? 'Reolink Duo 2 PoE' : 'RLC-823A', name: 'Porch', type: 'IPC', exactType: 'IPC', channelNum: chans.length, firmVer: 'v3.1.0.stand-in', hardVer: 'IPC_523', serial: '0001' } })
				: ok(c.cmd, { DevInfo: { model: 'RLN8-410', name: 'Stand-in NVR', type: 'NVR', exactType: 'NVR', channelNum: chans.length, firmVer: 'v3.3.0.stand-in', hardVer: 'H3MB18', serial: '0000' } });
		case 'GetChannelstatus':
			if (CAMERA) return err(c.cmd, -9, 'not support');
			return ok(c.cmd, { count: chans.length, status: chans.map((x, i) => ({ channel: i, name: x.name, online: x.online, typeInfo: x.type, uid: 'X' + i })) });
		case 'GetAbility':
			return ok(c.cmd, { Ability: { abilityChn: chans.map((x) => ({ floodLight: ver(!!x.led), alarmAudio: ver(x.ptz), ptzCtrl: ver(x.ptz), ptzPreset: ver(x.ptz), alarmIrLights: ver(true) })) } });
		case 'GetEvents': {
			if (!ch) return err(c.cmd, -4, 'param error');
			const s = (k) => ({ alarm_state: ch.ev[k] ? 1 : 0, support: ch.support[k] ? 1 : 0 });
			return ok(c.cmd, { channel: p.channel, md: { alarm_state: ch.ev.md ? 1 : 0, support: 1 }, ai: { people: s('people'), vehicle: s('vehicle'), dog_cat: s('dog_cat'), package: s('package') }, visitor: s('visitor') });
		}
		case 'GetPtzPreset':
			return ch?.ptz ? ok(c.cmd, { PtzPreset: presets }) : err(c.cmd, -9, 'not support');
		case 'GetWhiteLed':
			return ch?.led ? ok(c.cmd, { WhiteLed: { channel: p.channel, ...ch.led, mode: 0 } }) : err(c.cmd, -9, 'not support');
		case 'SetWhiteLed': {
			if (!ch?.led) return err(c.cmd, -9, 'not support');
			ch.led.state = p.WhiteLed.state;
			if (p.WhiteLed.bright !== undefined) ch.led.bright = p.WhiteLed.bright;
			return ok(c.cmd, { rspCode: 200 });
		}
		case 'GetIrLights':
			return ok(c.cmd, { IrLights: { channel: p.channel, state: ch.ir } });
		case 'SetIrLights':
			ch.ir = p.IrLights.state;
			return ok(c.cmd, { rspCode: 200 });
		case 'AudioAlarmPlay':
			return ch?.ptz ? ok(c.cmd, { rspCode: 200 }) : err(c.cmd, -9, 'not support');
		case 'PtzCtrl':
			return ch?.ptz ? ok(c.cmd, { rspCode: 200 }) : err(c.cmd, -9, 'not support');
		case 'Logout':
			sessions.delete(token);
			return ok(c.cmd, { rspCode: 200 });
		default:
			return err(c.cmd, -9, 'not support');
	}
}

function login(c) {
	if (Date.now() < lockedUntil) return err('Login', -31, 'login locked, please wait');
	const u = c.param?.User ?? {};
	if (u.userName !== USER || u.password !== PASS) {
		if (++refused >= 5) lockedUntil = Date.now() + 300_000;
		return err('Login', -7, 'login failed');
	}
	refused = 0;
	for (const [t, until] of sessions) if (until < Date.now()) sessions.delete(t);
	if (sessions.size >= MAX_SESSIONS) return err('Login', -5, 'max session');
	const name = randomBytes(8).toString('hex');
	sessions.set(name, Date.now() + 3600_000);
	return ok('Login', { Token: { leaseTime: 3600, name } });
}

http
	.createServer((req, res) => {
		const url = new URL(req.url, 'http://x');
		let body = '';
		req.on('data', (d) => (body += d));
		req.on('end', () => {
			const send = (code, v, type = 'application/json') => {
				res.writeHead(code, { 'content-type': type });
				res.end(type === 'application/json' ? JSON.stringify(v) : v);
			};
			if (url.pathname === '/standin/set') {
				const b = JSON.parse(body || '{}');
				const ch = chans[b.channel ?? 0];
				for (const k of Object.keys(ch.ev)) if (k in b) ch.ev[k] = b[k] ? 1 : 0;
				if ('online' in b) ch.online = b.online ? 1 : 0;
				return send(200, { ok: true, ev: ch.ev });
			}
			if (url.pathname === '/standin/log') return send(200, log.slice(-50));
			if (url.pathname === '/standin/state') return send(200, { sessions: sessions.size, chans, lockedUntil });
			if (url.pathname !== '/cgi-bin/api.cgi') return send(404, {});
			const cmd = url.searchParams.get('cmd');
			const token = url.searchParams.get('token');
			if (cmd === 'Snap') {
				log.push({ cmd: 'Snap', channel: url.searchParams.get('channel') });
				if (!sessions.has(token)) return send(200, [err('Snap', -6, 'please login first')]);
				return send(200, JPEG, 'image/jpeg');
			}
			let cmds;
			try {
				cmds = JSON.parse(body || '[]');
			} catch {
				return send(200, [err(cmd, -1, 'bad json')]);
			}
			if (cmd === 'Login') return send(200, [login(cmds[0])]);
			const out = cmds.map((c) => {
				if (!sessions.has(token)) return err(c.cmd, -6, 'please login first');
				if (!c.cmd.startsWith('Get')) log.push({ cmd: c.cmd, param: c.param });
				return answer(c, token);
			});
			send(200, out);
		});
	})
	.listen(PORT, '127.0.0.1', () => console.log(`Reolink stand-in ${CAMERA ? `camera (${LENSES} lens)` : 'NVR'} on ${PORT}`));
