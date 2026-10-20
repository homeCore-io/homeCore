<svelte:options
	customElement={{
		tag: 'hc-lutron-keypad',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A keypad's panel: the keypad as it is on the wall, its lights live.
     Pick a button to press it or set its light; below, what was pressed
     last and the automations that start on the picked button. The app
     draws the keypad's name and room above. -->
<script>
	import Face from './lib/Face.svelte';
	import {
		adopt,
		ago,
		automationsOn,
		buttonsOf,
		LED_STATES,
		ledButtonsIn,
		ledOf,
		loadRules,
		presses,
		schemaOf
	} from './lib/lutron.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const buttons = $derived(buttonsOf(dev));
	const canPress = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);

	// Which buttons have a light: the schema says, before any has reported.
	let lit = $state([]);
	$effect(() => {
		if (hc && deviceId) void schemaOf(hc, deviceId).then((s) => (lit = ledButtonsIn(s)));
	});
	const led = (n) => ledOf(dev, n) ?? (lit.includes(n) ? 0 : undefined);

	// The button picked, on this keypad: another device starts at the first.
	let picked = $state(null);
	const pick = (n) => (picked = { deviceId, n });
	const sel = $derived(
		buttons.find((b) => picked?.deviceId === deviceId && b.number === picked.n) ?? buttons[0]
	);
	const hasLed = $derived(sel ? led(sel.number) !== undefined : false);

	// What was pressed lately, from history; read again when a press lands.
	let log = $state([]);
	const stamp = $derived(buttons.map((b) => `${dev?.attributes?.[`button_${b.number}`] ?? ''}`).join('|') + (dev?.attributes?.last_button ?? ''));
	$effect(() => {
		void stamp;
		if (!hc || !deviceId || !buttons.length) return;
		const t = setTimeout(() => void presses(hc, deviceId, buttons).then((p) => (log = p.slice(0, 5))), 400);
		return () => clearTimeout(t);
	});

	let rules = $state([]);
	$effect(() => {
		if (hc) void loadRules(hc).then((r) => (rules = r));
	});
	const mine = $derived(sel ? automationsOn(rules, deviceId, sel.number) : []);
	const canWrite = $derived(!!hc?.me.can('automations:write'));

	let busy = $state('');
	let error = $state('');
	async function send(body, what) {
		busy = what;
		error = '';
		const r = await hc.command(deviceId, body);
		if (!r.ok) error = r.error;
		busy = '';
	}
	const press = () => send({ action: 'press_button', button: sel.number }, 'press');
	const setLed = (v) => send({ action: 'set_led', button: sel.number, state: v }, `led${v}`);
</script>

{#if dev}
	<div class="panel">
		{#if buttons.length}
			<div class="top">
				<Face {buttons} {led} selected={sel?.number} onpick={pick} label={`${hc.devices.label(dev)}'s buttons`} />
				{#if sel}
					<div class="side">
						<div class="what">
							<span class="hc-quiet">Button {sel.number}</span>
							<span class="name">{sel.name}</span>
						</div>
						<div class="col">
							<button class="hc-pill go big" disabled={!canPress || busy === 'press'} onclick={press}>{busy === 'press' ? 'Pressing…' : 'Press'}</button>
							<span class="hint">Does what the keypad does when it's pressed.</span>
						</div>
						{#if hasLed}
							<div class="col">
								<span class="sub">Its light</span>
								<div class="choices" role="radiogroup" aria-label={`${sel.name}'s light`}>
									{#each LED_STATES as c (c.value)}
										<button
											class="choice"
											class:on={led(sel.number) === c.value}
											role="radio"
											aria-checked={led(sel.number) === c.value}
											disabled={!canPress || busy !== ''}
											onclick={() => setLed(c.value)}>{c.name}</button
										>
									{/each}
								</div>
								<span class="hint">The repeater may change it back when its scene changes.</span>
							</div>
						{:else}
							<span class="hint">This button has no light of its own.</span>
						{/if}
					</div>
				{/if}
			</div>
			{#if error}<p class="err" role="alert">{error}</p>{/if}
		{:else}
			<p class="hc-quiet">The plugin hasn't said which buttons this keypad has. Read the repeater's layout in the plugin's settings.</p>
		{/if}

		<div class="col">
			<span class="sub">Last pressed</span>
			{#if log.length}
				<div class="list">
					{#each log as p (`${p.at}${p.number}`)}
						<div class="row"><span>{p.name}, {p.how}</span><span class="hc-quiet">{ago(p.at)}</span></div>
					{/each}
				</div>
			{:else}
				<p class="hc-quiet">No presses seen yet.</p>
			{/if}
		</div>

		{#if sel && hc.me.can('automations:read')}
			<div class="col">
				<span class="sub">Automations on this button</span>
				{#each mine as r (r.id)}
					<button class="link-row" onclick={() => hc.ui.navigate(`/settings/automations/${encodeURIComponent(r.id)}`)}
						><span>{r.name}</span><span class="open">Open ›</span></button
					>
				{:else}
					<p class="hc-quiet">None yet.</p>
				{/each}
				{#if canWrite}
					<button class="link" onclick={() => hc.ui.navigate('/settings/automations/new')}>+ When {sel.name} is pressed…</button>
				{/if}
			</div>
		{/if}
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 18px;
		container-type: inline-size;
	}
	.top {
		display: flex;
		gap: 20px;
		align-items: flex-start;
	}
	.side {
		flex: 1;
		min-width: 0;
		max-width: 340px;
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.what {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.what .hc-quiet {
		font-size: 12px;
	}
	.name {
		font-size: 18px;
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.col {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.sub {
		font-size: 13px;
		font-weight: 600;
	}
	.hint {
		font-size: 12px;
		color: var(--ink-muted);
	}
	.big {
		min-height: 44px;
		justify-content: center;
		width: 100%;
	}
	.choices {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px;
	}
	.choice {
		min-height: 40px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 14px;
	}
	.choice.on {
		background: color-mix(in srgb, var(--track-on) 10%, var(--surface));
		border-color: color-mix(in srgb, var(--track-on) 60%, var(--surface));
		font-weight: 600;
	}
	.list {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
		overflow: hidden;
	}
	.row {
		display: flex;
		justify-content: space-between;
		gap: 12px;
		padding: 10px 14px;
		font-size: 14px;
	}
	.row + .row {
		border-top: 1px solid var(--card-line);
	}
	.row .hc-quiet {
		font-size: 14px;
		white-space: nowrap;
	}
	.link-row {
		display: flex;
		justify-content: space-between;
		gap: 12px;
		padding: 12px 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius, 14px);
		background: var(--surface);
		font-size: 14px;
		text-align: left;
	}
	.open,
	.link {
		color: var(--accent);
	}
	.link {
		align-self: flex-start;
		border: none;
		background: none;
		padding: 0;
		font-size: 14px;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	@container (max-width: 380px) {
		.top {
			flex-direction: column;
			align-items: stretch;
		}
	}
</style>
