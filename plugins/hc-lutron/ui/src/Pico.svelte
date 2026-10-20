<svelte:options
	customElement={{
		tag: 'hc-lutron-pico',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A Pico's panel, drawn from the buttons it reports: the 2-button
     On/Off, the 5-button On/Raise/Favorite/Lower/Off, or whatever else it
     says it has. The last pressed is lit; what each button starts, and the
     last press. A Pico takes no commands, so nothing here sends any. -->
<script>
	import { adopt, ago, automationsOn, buttonsOf, loadRules, pressWord, presses } from './lib/lutron.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const buttons = $derived(buttonsOf(dev));

	// The order a Pico has them, top to bottom: On, Raise, Favorite, Lower, Off.
	const PLACE = { 2: 0, 5: 1, 3: 2, 6: 3, 4: 4 };
	const keys = $derived([...buttons].sort((x, y) => (PLACE[x.number] ?? 10 + x.number) - (PLACE[y.number] ?? 10 + y.number)));
	const small = (n) => buttons.length > 2 && (n === 3 || n === 5 || n === 6);
	const short = (name) => (name === 'Favorite' ? 'Fav' : name);

	// The last press, with its time, from history.
	let last = $state(null);
	const stamp = $derived(`${a.last_button ?? ''}|${buttons.map((b) => a[`button_${b.number}`] ?? '').join('|')}`);
	$effect(() => {
		void stamp;
		if (!hc || !deviceId || !buttons.length) return;
		const t = setTimeout(() => void presses(hc, deviceId, buttons, 10).then((p) => (last = p[0] ?? null)), 400);
		return () => clearTimeout(t);
	});
	const lastNumber = $derived(last?.number ?? a.last_button);
	const lastLine = $derived(
		last ? `${last.name}, ${last.how}` : a.last_button_name ? `${a.last_button_name}, ${pressWord(a[`button_${a.last_button}`])}` : ''
	);

	let rules = $state([]);
	$effect(() => {
		if (hc) void loadRules(hc).then((r) => (rules = r));
	});
	// One line per button; a 5-button's raise and lower share one.
	const uses = $derived.by(() => {
		const rows = [];
		const rl = keys.filter((b) => b.number === 5 || b.number === 6);
		for (const b of keys) {
			if (rl.length === 2 && (b.number === 5 || b.number === 6)) continue;
			rows.push({ name: b.name, numbers: [b.number] });
		}
		if (rl.length === 2) rows.push({ name: `${rl[0].name} · ${rl[1].name}`, numbers: rl.map((b) => b.number) });
		return rows.map((r) => ({ ...r, rules: r.numbers.flatMap((n) => automationsOn(rules, deviceId, n)) }));
	});
	const canWrite = $derived(!!hc?.me.can('automations:write'));
	const open = (id) => hc.ui.navigate(`/settings/automations/${encodeURIComponent(id)}`);
</script>

{#if dev}
	<div class="panel">
		{#if buttons.length}
			<span class="hc-quiet">{buttons.length}-button Pico</span>
			<div class="draw">
				<div class="pico" aria-label={`${hc.devices.label(dev)}'s buttons`} role="img">
					{#each keys as b (b.number)}
						<span class="key" class:small={small(b.number)} class:lit={lastNumber === b.number}>{short(b.name)}</span>
					{/each}
				</div>
				<span class="hc-quiet">The lit button was pressed last.</span>
			</div>

			{#if hc.me.can('automations:read')}
				<div class="col">
					<span class="sub">What its buttons do</span>
					<div class="list">
						{#each uses as u (u.name)}
							<div class="row">
								<span>{u.name}</span>
								<span class="does">
									{#each u.rules as r (r.id)}<button class="link" onclick={() => open(r.id)}>{r.name} ›</button>{/each}
									{#if !u.rules.length}
										{#if canWrite}<button class="link" onclick={() => hc.ui.navigate('/settings/automations/new')}>+ Make one</button
											>{:else}<span class="hc-quiet">Nothing yet</span>{/if}
									{/if}
								</span>
							</div>
						{/each}
					</div>
				</div>
			{/if}

			<div class="row box">
				<span>{lastLine ? `Last: ${lastLine}` : 'No presses seen yet'}</span>
				{#if last}<span class="hc-quiet">{ago(last.at)}</span>{/if}
			</div>
		{:else}
			<p class="hc-quiet">The plugin hasn't said which buttons this Pico has. Read the repeater's layout in the plugin's settings.</p>
		{/if}
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.panel > .hc-quiet {
		font-size: 12px;
	}
	.draw {
		display: flex;
		gap: 18px;
		align-items: center;
	}
	.draw .hc-quiet {
		max-width: 300px;
	}
	.pico {
		flex: none;
		width: 84px;
		padding: 16px 10px;
		border-radius: 42px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 60%, var(--surface));
		border: 1px solid var(--line);
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
	}
	.key {
		width: 60px;
		height: 42px;
		display: flex;
		align-items: center;
		justify-content: center;
		border-radius: 10px;
		background: var(--sheet, var(--surface));
		border: 1px solid color-mix(in srgb, var(--line) 80%, var(--ink) 8%);
		font-size: 11px;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		overflow: hidden;
	}
	.key.small {
		height: 30px;
	}
	.key.lit {
		background: color-mix(in srgb, var(--track-on) 10%, var(--surface));
		border: 2px solid var(--track-on);
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
	.list {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--card-line);
		border-radius: 12px;
		overflow: hidden;
		font-size: 14px;
	}
	.row {
		display: flex;
		justify-content: space-between;
		gap: 10px;
		padding: 9px 12px;
		font-size: 14px;
	}
	.list .row + .row {
		border-top: 1px solid var(--card-line);
	}
	.box {
		border: 1px solid var(--card-line);
		border-radius: 12px;
	}
	.box .hc-quiet {
		font-size: 14px;
		white-space: nowrap;
	}
	.does {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: 2px;
		min-width: 0;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-size: 14px;
		text-align: right;
	}
</style>
