<svelte:options
	customElement={{
		tag: 'hc-lutron-vcrx',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A visor control receiver's panel: its buttons (the car visor's and its
     own) with their lights, to press; its contact inputs, open or closed and
     since when; and what was pressed last. -->
<script>
	import {
		adopt,
		ago,
		buttonsOf,
		inputsOf,
		ledButtonsIn,
		ledOf,
		presses,
		schemaOf,
		settingsHref,
		since
	} from './lib/lutron.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const buttons = $derived(buttonsOf(dev));
	const canPress = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);

	let schema = $state(null);
	$effect(() => {
		if (hc && deviceId) void schemaOf(hc, deviceId).then((s) => (schema = s));
	});
	const lit = $derived(ledButtonsIn(schema));
	const led = (n) => ledOf(dev, n) ?? (lit.includes(n) ? 0 : undefined);
	const inputs = $derived(inputsOf(dev, schema));

	// Since when each input has read what it reads.
	let sinces = $state({});
	const inputStamp = $derived(inputs.map((i) => `${i.attribute}=${a[i.attribute]}`).join('|'));
	$effect(() => {
		void inputStamp;
		if (!hc || !deviceId) return;
		for (const i of inputs) void since(hc, deviceId, i.attribute).then((t) => (sinces = { ...sinces, [i.attribute]: t }));
	});

	let last = $state(null);
	const stamp = $derived(`${a.last_button ?? ''}|${buttons.map((b) => a[`button_${b.number}`] ?? '').join('|')}`);
	$effect(() => {
		void stamp;
		if (!hc || !deviceId || !buttons.length) return;
		const t = setTimeout(() => void presses(hc, deviceId, buttons, 10).then((p) => (last = p[0] ?? null)), 400);
		return () => clearTimeout(t);
	});

	let busy = $state(0);
	let error = $state('');
	async function press(n) {
		busy = n;
		error = '';
		const r = await hc.command(deviceId, { action: 'press_button', button: n });
		if (!r.ok) error = r.error;
		busy = 0;
	}
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));
</script>

{#if dev}
	<div class="panel">
		<div class="col">
			<span class="sub">Its buttons</span>
			<span class="hc-quiet">The car visor's buttons and the receiver's own, as the repeater programmed them. Tap one to press it.</span>
			<div class="grid">
				{#each buttons as b (b.number)}
					{@const s = led(b.number)}
					<button class="btn" disabled={!canPress || busy === b.number} aria-label={`Press ${b.name}`} onclick={() => press(b.number)}>
						{#if s !== undefined}<span class="led s{s}"></span>{/if}<span>{b.name}</span>
					</button>
				{/each}
			</div>
			{#if error}<p class="err" role="alert">{error}</p>{/if}
		</div>

		<div class="col">
			<span class="sub">Contact inputs</span>
			{#if inputs.length}
				<div class="inputs">
					{#each inputs as i (i.component)}
						{@const v = a[i.attribute]}
						<div class="input">
							<span class="hc-quiet">{i.name}</span>
							<span class="state">{v === true ? 'Open' : v === false ? 'Closed' : 'Not heard yet'}</span>
							<span class="when">{v !== undefined && sinces[i.attribute] ? `since ${ago(sinces[i.attribute])}` : ''}</span>
						</div>
					{/each}
				</div>
				<span class="hc-quiet">Wired to a door or sensor, an input reads Open or Closed.</span>
			{:else}
				<span class="hc-quiet"
					>Wired to a door or sensor, an input reads Open or Closed. None are set up on this receiver yet — add their numbers
					(32 and 33) to its contact inputs in {#if admin}<button class="link" onclick={() => hc.ui.navigate(settingsHref(hc))}
							>Settings</button
						>{:else}the plugin's settings{/if}.</span
				>
			{/if}
		</div>

		<div class="col">
			<span class="sub">Last pressed</span>
			<div class="row">
				<span>{last ? `${last.name}, ${last.how}` : (a.last_button_name ?? 'No presses seen yet')}</span>
				{#if last}<span class="hc-quiet">{ago(last.at)}</span>{/if}
			</div>
		</div>
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 18px;
	}
	.col {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.sub {
		font-size: 13px;
		font-weight: 600;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 8px;
	}
	.btn {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 48px;
		padding: 0 12px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--sheet, var(--surface));
		text-align: left;
		font-size: 14px;
	}
	.led {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--track-off);
	}
	.led.s1,
	.led.s2,
	.led.s3 {
		background: var(--track-on);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--track-on) 30%, transparent);
	}
	.inputs {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 8px;
	}
	.input {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 12px 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
	}
	.state {
		font-size: 18px;
		font-weight: 600;
	}
	.when {
		font-size: 12px;
		color: var(--ink-muted);
		min-height: 1em;
	}
	.row {
		display: flex;
		justify-content: space-between;
		gap: 12px;
		padding: 10px 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
		font-size: 14px;
	}
	.row .hc-quiet {
		font-size: 14px;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-size: inherit;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
</style>
