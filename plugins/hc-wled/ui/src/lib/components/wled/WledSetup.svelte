<!--
	How a WLED controller is set up (admins): its LED outputs and power, what
	it does at power-on, its buttons and IR remote, its name and address on
	the network, its Wi-Fi and its own access point, and its clock.

	Each part is sent on its own, in WLED's shape; WLED merges it. Hardware
	and network changes restart the controller, and are asked about first.
-->
<script lang="ts">
	import { ask } from '#lib/confirm.svelte.js';
	import type { Preset } from '#lib/wled.js';
	import { applySettings, ipList, ipText, TIME_ZONES } from '#lib/wledSettings.js';

	let {
		deviceId,
		sections,
		presets,
		attrs,
		reload
	}: {
		deviceId: string;
		sections: Record<string, unknown>;
		presets: Preset[];
		attrs: Record<string, unknown>;
		reload: () => Promise<void>;
	} = $props();

	let msg = $state('');
	let busy = $state(false);
	async function put(settings: Record<string, unknown>, restart = false, said = 'Saved.') {
		busy = true;
		const err = await applySettings(deviceId, settings, restart);
		busy = false;
		msg = err ?? (restart ? `${said} The controller is restarting.` : said);
		await reload();
	}
	async function putRestart(title: string, settings: Record<string, unknown>) {
		if (!(await ask({ title, go: 'Change and restart', stay: 'Not now' }))) {
			await reload();
			return;
		}
		await put(settings, true);
	}

	// ---- LED outputs ----
	type Output = {
		pin?: number[];
		type?: number;
		len?: number;
		start?: number;
		order?: number;
		rev?: boolean;
	};
	const outputs = $derived((sections.led_outputs as Output[] | undefined) ?? []);
	const ORDER = ['GRB', 'RGB', 'BRG', 'RBG', 'BGR', 'GBR'];
	const setOutput = (i: number, change: Partial<Output>) =>
		putRestart('Change the LED outputs? The controller restarts to take it.', {
			hw: { led: { ins: outputs.map((o, j) => (j === i ? { ...o, ...change } : o)) } }
		});
	const maxpwr = $derived(Number(sections.power_limit_ma ?? 0));

	// ---- power-on ----
	const def = $derived((sections.power_on ?? {}) as { ps?: number; on?: boolean; bri?: number });

	// ---- buttons and remote ----
	type Btn = { type?: number; pin?: number[]; macros?: number[] };
	const buttons = $derived((sections.buttons as Btn[] | undefined) ?? []);
	const BUTTONS: [number, string][] = [
		[0, 'Not used'],
		[2, 'Push button'],
		[3, 'Push button (active high)'],
		[4, 'Switch'],
		[5, 'Motion sensor (PIR)'],
		[6, 'Touch'],
		[9, 'Touch switch'],
		[7, 'Analog'],
		[8, 'Analog (inverted)']
	];
	const ACTS = ['Press', 'Long press', 'Double press'];
	const setButtons = (list: Btn[]) =>
		putRestart('Change the buttons? The controller restarts to take it.', {
			hw: { btn: { ins: list } }
		});
	const setButton = (i: number, change: Partial<Btn>) =>
		setButtons(buttons.map((b, j) => (j === i ? { ...b, ...change } : b)));
	const ir = $derived((sections.ir ?? {}) as { pin?: number; type?: number });
	const REMOTES = [
		'None',
		'24-key RGB',
		'24-key with white',
		'40-key blue',
		'44-key RGB',
		'21-key RGB',
		'6-key black',
		'9-key red',
		'Its own (ir.json)'
	];

	// ---- network ----
	type Wifi = { ssid?: string; ip?: number[]; gw?: number[]; sn?: number[] };
	const wifi = $derived(
		(((sections.network as { ins?: Wifi[] } | undefined)?.ins ?? [])[0] ?? {}) as Wifi
	);
	const ident = $derived((sections.identity ?? {}) as { mdns?: string; name?: string });
	const ap = $derived(
		(sections.access_point ?? {}) as { ssid?: string; chan?: number; hide?: number; behav?: number }
	);
	let ip = $state('');
	let gw = $state('');
	let sn = $state('');
	let ssid = $state('');
	let psk = $state('');
	$effect(() => {
		ip = ipText(wifi.ip);
		gw = ipText(wifi.gw);
		sn = ipText(wifi.sn) || '255.255.255.0';
	});
	async function saveAddress() {
		const [a, g, s] = [ipList(ip), ipList(gw), ipList(sn)];
		if (!a || !g || !s) return void (msg = 'An address is four numbers 0–255, like 10.0.10.80.');
		const dhcp = a.every((n) => n === 0);
		await putRestart(
			dhcp
				? 'Let the network give it an address (DHCP)? It restarts, and may come back at another address — change it on the plugin page if it does.'
				: `Give it the fixed address ${a.join('.')}? It restarts there. If the address is wrong for the network it can’t be reached, and needs its own access point or a reset to fix.`,
			{ nw: { ins: [{ ip: a, gw: g, sn: s }] } }
		);
	}
	async function saveWifi() {
		if (!ssid.trim() || !psk) return void (msg = 'Both the network and its password.');
		await putRestart(
			`Move it to the Wi-Fi network “${ssid.trim()}”? It restarts and joins it. If the name or password is wrong it opens its own access point instead.`,
			{ nw: { ins: [{ ssid: ssid.trim(), psk }] } }
		);
		psk = '';
	}
	const AP = [
		'If it can’t join Wi-Fi after starting',
		'Whenever it isn’t on Wi-Fi',
		'Always',
		'Never'
	];

	// ---- clock ----
	const ntp = $derived(
		(sections.clock ?? {}) as {
			en?: boolean;
			host?: string;
			tz?: number;
			ampm?: boolean;
			ln?: number;
			lt?: number;
		}
	);
</script>

<section class="wf">
	<div class="head">
		<h3>Setup</h3>
		<span class="num">hardware and network changes restart the controller</span>
	</div>

	<h4>LED outputs</h4>
	{#each outputs as o, i (i)}
		<div class="cols">
			<div class="f">
				<span class="label">Pin</span><span class="num">{(o.pin ?? []).join(', ')}</span>
			</div>
			<label class="f"
				><span class="label">LEDs</span>
				<input
					type="number"
					min="1"
					value={o.len ?? 0}
					disabled={busy}
					onchange={(e) => setOutput(i, { len: Number(e.currentTarget.value) })}
				/></label
			>
			<label class="f"
				><span class="label">Starts at</span>
				<input
					type="number"
					min="0"
					value={o.start ?? 0}
					disabled={busy}
					onchange={(e) => setOutput(i, { start: Number(e.currentTarget.value) })}
				/></label
			>
			<label class="f"
				><span class="label">Colour order</span>
				<select
					value={o.order ?? 0}
					disabled={busy}
					onchange={(e) => setOutput(i, { order: Number(e.currentTarget.value) })}
				>
					{#each ORDER as name, n (name)}<option value={n}>{name}</option>{/each}
				</select></label
			>
			<label class="c"
				><input
					type="checkbox"
					checked={o.rev === true}
					disabled={busy}
					onchange={(e) => setOutput(i, { rev: e.currentTarget.checked })}
				/>Reversed</label
			>
		</div>
	{/each}
	<div class="cols">
		<label class="f"
			><span class="label">Power limit (mA, 0 for none)</span>
			<input
				type="number"
				min="0"
				step="50"
				value={maxpwr}
				disabled={busy}
				onchange={(e) =>
					putRestart('Change the power limit? The controller restarts to take it.', {
						hw: { led: { maxpwr: Number(e.currentTarget.value) } }
					})}
			/></label
		>
	</div>

	<h4>At power-on</h4>
	<div class="cols">
		<label class="c"
			><input
				type="checkbox"
				checked={def.on === true}
				disabled={busy}
				onchange={(e) => put({ def: { on: e.currentTarget.checked } })}
			/>Comes on by itself</label
		>
		<label class="f"
			><span class="label">With</span>
			<select
				value={def.ps ?? 0}
				disabled={busy}
				onchange={(e) => put({ def: { ps: Number(e.currentTarget.value) } })}
			>
				<option value={0}>What it last showed</option>
				{#each presets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
			</select></label
		>
		<label class="f"
			><span class="label"
				>Brightness · <span class="num">{Math.round(((def.bri ?? 128) / 255) * 100)}%</span></span
			>
			<input
				type="range"
				min="1"
				max="255"
				value={def.bri ?? 128}
				disabled={busy}
				onchange={(e) => put({ def: { bri: Number(e.currentTarget.value) } })}
			/></label
		>
	</div>

	<h4>Buttons and switches</h4>
	{#each buttons as b, i (i)}
		<div class="item">
			<div class="cols">
				<label class="f"
					><span class="label">Kind</span>
					<select
						value={b.type ?? 0}
						disabled={busy}
						onchange={(e) => setButton(i, { type: Number(e.currentTarget.value) })}
					>
						{#each BUTTONS as [v, name] (v)}<option value={v}>{name}</option>{/each}
					</select></label
				>
				<label class="f"
					><span class="label">Pin</span>
					<input
						type="number"
						min="-1"
						max="48"
						value={(b.pin ?? [-1])[0]}
						disabled={busy}
						onchange={(e) => setButton(i, { pin: [Number(e.currentTarget.value)] })}
					/></label
				>
				{#each ACTS as act, n (act)}
					<label class="f"
						><span class="label">{act}</span>
						<select
							value={(b.macros ?? [0, 0, 0])[n] ?? 0}
							disabled={busy}
							onchange={(e) =>
								setButton(i, {
									macros: [0, 1, 2].map((k) =>
										k === n ? Number(e.currentTarget.value) : ((b.macros ?? [])[k] ?? 0)
									)
								})}
						>
							<option value={0}
								>{n === 0 ? 'On / off' : n === 1 ? 'Brighter, dimmer' : 'Nightlight'}</option
							>
							{#each presets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
						</select></label
					>
				{/each}
			</div>
			<div class="buttons">
				<button
					class="pill danger"
					disabled={busy}
					onclick={() => setButtons(buttons.filter((_, j) => j !== i))}>Remove</button
				>
			</div>
		</div>
	{/each}
	<div class="buttons">
		<button
			class="pill"
			disabled={busy}
			onclick={() => setButtons([...buttons, { type: 2, pin: [-1], macros: [0, 0, 0] }])}
			>Add a button</button
		>
	</div>
	<div class="cols">
		<label class="f"
			><span class="label">IR remote</span>
			<select
				value={ir.type ?? 0}
				disabled={busy}
				onchange={(e) =>
					putRestart('Change the IR remote? The controller restarts to take it.', {
						hw: { ir: { type: Number(e.currentTarget.value) } }
					})}
			>
				{#each REMOTES as r, n (r)}<option value={n}>{r}</option>{/each}
			</select></label
		>
		{#if (ir.type ?? 0) !== 0}
			<label class="f"
				><span class="label">Receiver pin</span>
				<input
					type="number"
					min="-1"
					max="48"
					value={ir.pin ?? -1}
					disabled={busy}
					onchange={(e) =>
						putRestart('Change the IR receiver pin? The controller restarts to take it.', {
							hw: { ir: { pin: Number(e.currentTarget.value) } }
						})}
				/></label
			>
		{/if}
	</div>

	<h4>Network</h4>
	<div class="cols">
		<label class="f"
			><span class="label">Name on the network</span>
			<input
				type="text"
				value={ident.mdns ?? ''}
				disabled={busy}
				onchange={(e) =>
					putRestart(
						`Call it ${e.currentTarget.value.trim()}.local? It restarts under that name; homeCore keeps reaching it at its address.`,
						{ id: { mdns: e.currentTarget.value.trim() } }
					)}
			/></label
		>
		<div class="f">
			<span class="label">Wi-Fi now</span>
			<span class="num"
				>{wifi.ssid ?? '—'} · {attrs.wifi_signal_pct ?? '—'}% · channel {attrs.wifi_channel ??
					'—'}</span
			>
		</div>
	</div>
	<div class="cols">
		<label class="f"
			><span class="label">Fixed address (empty: from DHCP)</span>
			<input type="text" bind:value={ip} placeholder="from DHCP" disabled={busy} /></label
		>
		<label class="f"
			><span class="label">Gateway</span>
			<input type="text" bind:value={gw} disabled={busy || !ip.trim()} /></label
		>
		<label class="f"
			><span class="label">Subnet mask</span>
			<input type="text" bind:value={sn} disabled={busy || !ip.trim()} /></label
		>
	</div>
	<div class="buttons">
		<button
			class="pill"
			disabled={busy || (ip === ipText(wifi.ip) && gw === ipText(wifi.gw))}
			onclick={saveAddress}>Use this address</button
		>
	</div>
	<details>
		<summary class="label">Move it to another Wi-Fi network</summary>
		<div class="cols">
			<label class="f"
				><span class="label">Network</span><input
					type="text"
					bind:value={ssid}
					disabled={busy}
				/></label
			>
			<label class="f"
				><span class="label">Password</span><input
					type="password"
					autocomplete="new-password"
					bind:value={psk}
					disabled={busy}
				/></label
			>
		</div>
		<div class="buttons">
			<button class="pill" disabled={busy || !ssid.trim() || !psk} onclick={saveWifi}
				>Move it</button
			>
		</div>
	</details>
	<div class="cols">
		<label class="f"
			><span class="label">Its own access point</span>
			<select
				value={ap.behav ?? 0}
				disabled={busy}
				onchange={(e) => put({ ap: { behav: Number(e.currentTarget.value) } })}
			>
				{#each AP as label, n (label)}<option value={n}>{label}</option>{/each}
			</select></label
		>
		<label class="f"
			><span class="label">Its name</span>
			<input
				type="text"
				value={ap.ssid ?? ''}
				disabled={busy}
				onchange={(e) => put({ ap: { ssid: e.currentTarget.value.trim() } })}
			/></label
		>
		<label class="c"
			><input
				type="checkbox"
				checked={ap.hide === 1}
				disabled={busy}
				onchange={(e) => put({ ap: { hide: e.currentTarget.checked ? 1 : 0 } })}
			/>Hidden</label
		>
	</div>

	<h4>Clock</h4>
	<div class="cols">
		<label class="c"
			><input
				type="checkbox"
				checked={ntp.en === true}
				disabled={busy}
				onchange={(e) => put({ if: { ntp: { en: e.currentTarget.checked } } })}
			/>Set from the internet</label
		>
		<label class="f"
			><span class="label">Time server</span>
			<input
				type="text"
				value={ntp.host ?? ''}
				disabled={busy || !ntp.en}
				onchange={(e) => put({ if: { ntp: { host: e.currentTarget.value.trim() } } })}
			/></label
		>
		<label class="f"
			><span class="label">Time zone</span>
			<select
				value={ntp.tz ?? 0}
				disabled={busy}
				onchange={(e) => put({ if: { ntp: { tz: Number(e.currentTarget.value) } } })}
			>
				{#each TIME_ZONES as z, n (z)}<option value={n}>{z}</option>{/each}
				{#if (ntp.tz ?? 0) >= TIME_ZONES.length}<option value={ntp.tz}>Zone {ntp.tz}</option>{/if}
			</select></label
		>
		<label class="c"
			><input
				type="checkbox"
				checked={ntp.ampm === true}
				disabled={busy}
				onchange={(e) => put({ if: { ntp: { ampm: e.currentTarget.checked } } })}
			/>12-hour clock</label
		>
	</div>
	<div class="cols">
		<label class="f"
			><span class="label">Latitude (for sunrise and sunset)</span>
			<input
				type="number"
				step="0.01"
				min="-90"
				max="90"
				value={ntp.lt ?? 0}
				disabled={busy}
				onchange={(e) => put({ if: { ntp: { lt: Number(e.currentTarget.value) } } })}
			/></label
		>
		<label class="f"
			><span class="label">Longitude</span>
			<input
				type="number"
				step="0.01"
				min="-180"
				max="180"
				value={ntp.ln ?? 0}
				disabled={busy}
				onchange={(e) => put({ if: { ntp: { ln: Number(e.currentTarget.value) } } })}
			/></label
		>
	</div>
	{#if msg}<p class="quiet" role="status">{msg}</p>{/if}
</section>
