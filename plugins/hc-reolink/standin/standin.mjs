// A stand-in Reolink for the sandbox, answering /cgi-bin/api.cgi in Reolink's
// shapes. By default an NVR with two cameras: channel 0 a doorbell (person,
// package, visitor), channel 1 a PTZ camera with spotlight, siren, infrared
// and presets (person, vehicle, animal). MODE=camera makes it a camera on its
// own (type IPC, no channel list): one PTZ camera; LENSES=2 makes it report
// two channels, as a Duo may, which the plugin still shows as one camera. Sessions are limited, as a real device's are, and refused sign-ins
// lock it for a while. PUSH_PORT starts its push (Baichuan): it signs in a
// client as Reolink does, takes a subscription, and pushes an AlarmEventList
// whenever /standin/set changes what a camera sees.
//
//   POST /standin/set  {"channel":1,"md":1,"people":1,...}  what a camera sees
//   GET  /standin/log  the commands it was sent
//   GET  /standin/state
import http from 'node:http';
import net from 'node:net';
import crypto from 'node:crypto';
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
	{ name: 'Stand-in doorbell', type: 'Reolink Video Doorbell', online: 1, ev: { md: 0, people: 0, vehicle: 0, dog_cat: 0, package: 0, visitor: 0 },
		support: { people: 1, vehicle: 0, dog_cat: 0, package: 1, visitor: 1 }, led: null, ir: 'Auto', ptz: false },
	{ name: 'Stand-in driveway', type: 'RLC-823A', online: 1, ev: { md: 0, people: 0, vehicle: 0, dog_cat: 0, package: 0, visitor: 0 },
		support: { people: 1, vehicle: 1, dog_cat: 1, package: 0, visitor: 0 }, led: { state: 0, bright: 80 }, ir: 'Auto', ptz: true }
];
// A camera on its own is the PTZ camera, once per lens.
if (CAMERA) {
	const cam = chans[1];
	chans.length = 0;
	for (let i = 0; i < LENSES; i++) chans.push({ ...structuredClone(cam), name: 'Stand-in porch' });
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
				? ok(c.cmd, { DevInfo: { model: LENSES > 1 ? 'Reolink Duo 2 PoE' : 'RLC-823A', name: 'Stand-in porch', type: 'IPC', exactType: 'IPC', channelNum: chans.length, firmVer: 'v3.1.0.stand-in', hardVer: 'IPC_523', serial: '0001' } })
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
				pushAlarm(b.channel ?? 0);
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
				if (!c.cmd.startsWith('Get')) log.push({ cmd: c.cmd, param: c.param, at: Date.now() });
				return answer(c, token);
			});
			send(200, out);
		});
	})
	.listen(PORT, '127.0.0.1', () => console.log(`Reolink stand-in ${CAMERA ? `camera (${LENSES} lens)` : 'NVR'} on ${PORT}`));

// ---- push (Baichuan) ----
const MAGIC = Buffer.from('f0debc0a', 'hex');
const XML_KEY = [0x1f, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0xff];
const bcXor = (buf, off) => Buffer.from([...buf].map((b, i) => b ^ (XML_KEY[(off + i) % 8] ^ off)));
const md5u = (t) => crypto.createHash('md5').update(t).digest('hex').slice(0, 31).toUpperCase();
const aes = (key, body) => {
	const c = crypto.createCipheriv('aes-128-cfb', key, Buffer.from('0123456789abcdef'));
	return Buffer.concat([c.update(body), c.final()]);
};
const subscribers = new Set();
function header(cmd, len, idBytes, status, cls) {
	const h = Buffer.alloc(cls === '1466' ? 20 : 24);
	MAGIC.copy(h, 0);
	h.writeUInt32LE(cmd, 4);
	h.writeUInt32LE(len, 8);
	idBytes.copy(h, 12);
	if (cls === '1466') Buffer.from('01dd1466', 'hex').copy(h, 16);
	else {
		h.writeUInt16LE(status, 16);
		Buffer.from(cls, 'hex').copy(h, 18);
		h.writeUInt32LE(len, 20);
	}
	return h;
}
function alarmXml(i) {
	const ev = chans[i].ev;
	const status = [ev.md && 'MD', ev.visitor && 'visitor'].filter(Boolean).join(',') || 'none';
	const ai = ['people', 'vehicle', 'dog_cat', 'package'].filter((k) => ev[k]).join(',') || 'none';
	return `<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<AlarmEventList version="1.1">\n<AlarmEvent version="1.1">\n<channelId>${i}</channelId>\n<status>${status}</status>\n<AItype>${ai}</AItype>\n<recording>0</recording>\n<timeStamp>0</timeStamp>\n</AlarmEvent>\n</AlarmEventList>\n</body>\n`;
}
function pushAlarm(i) {
	for (const sub of subscribers) {
		const body = aes(sub.key, Buffer.from(alarmXml(i)));
		const id = Buffer.from([251, 0, 0, 0]);
		sub.socket.write(Buffer.concat([header(33, body.length, id, 200, '1464'), body]));
	}
}
if (process.env.PUSH_PORT) {
	net
		.createServer((socket) => {
			let buf = Buffer.alloc(0);
			let nonce = '';
			let me = null;
			socket.on('data', (d) => {
				buf = Buffer.concat([buf, d]);
				for (;;) {
					if (buf.length < 20) return;
					const cmd = buf.readUInt32LE(4);
					const len = buf.readUInt32LE(8);
					const cls = buf.subarray(18, 20).toString('hex');
					const hl = cls === '1465' || cls === '1466' ? 20 : 24;
					if (buf.length < hl + len) return;
					const idBytes = Buffer.from(buf.subarray(12, 16));
					const body = buf.subarray(hl, hl + len);
					buf = buf.subarray(hl + len);
					if (cmd === 1 && len === 0) {
						nonce = randomBytes(6).toString('hex');
						const xml = Buffer.from(`<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<Encryption version="1.1">\n<type>md5</type>\n<nonce>${nonce}</nonce>\n</Encryption>\n</body>\n`);
						const enc = bcXor(xml, idBytes[0]);
						socket.write(Buffer.concat([header(1, enc.length, idBytes, 0, '1466'), enc]));
					} else if (cmd === 1) {
						const xml = bcXor(body, idBytes[0]).toString();
						const user = /<userName>(.*?)<\/userName>/.exec(xml)?.[1];
						const pass = /<password>(.*?)<\/password>/.exec(xml)?.[1];
						const ok = user === md5u(USER + nonce) && pass === md5u(PASS + nonce);
						log.push({ cmd: 'PushLogin', ok });
						socket.write(header(1, 0, idBytes, ok ? 200 : 401, '1464'));
						if (ok) me = { socket, key: Buffer.from(md5u(`${nonce}-${PASS}`).slice(0, 16)) };
					} else if (cmd === 31 && me) {
						subscribers.add(me);
						log.push({ cmd: 'PushSubscribe' });
						socket.write(header(31, 0, idBytes, 200, '1464'));
					} else if (cmd === 93 && me) {
						socket.write(header(93, 0, idBytes, 200, '1464'));
					}
				}
			});
			socket.on('close', () => me && subscribers.delete(me));
			socket.on('error', () => me && subscribers.delete(me));
		})
		.listen(Number(process.env.PUSH_PORT), '127.0.0.1', () => console.log(`push on ${process.env.PUSH_PORT}`));
}
