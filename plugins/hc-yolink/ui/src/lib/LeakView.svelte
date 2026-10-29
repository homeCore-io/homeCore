<!-- A leak sensor: dry or wet, when it was last wet and for how long; when
     it last checked in; its own temperature; whether its probe works; and
     what happens if it gets wet — the automations that start on it. -->
<script>
	import { ago, batteryLow, clock, lasted, lastWet, leakAutomations, listWords, quietAfter, temp, when } from './yolink.js';

	let { hc, dev, status = null } = $props();
	let alerts = $state();
	$effect(() => hc?.alerts?.subscribe((list) => (alerts = list)));

	const a = $derived(dev?.attributes ?? {});
	const off = $derived(dev?.available === false);
	const wet = $derived(a.water_detected === true);
	let unitsTick = $state(0);
	$effect(() => hc?.units?.subscribe?.(() => unitsTick++));

	let spell = $state(undefined);
	let autos = $state([]);
	const id = $derived(dev?.device_id);
	$effect(() => {
		if (!hc || !id) return;
		void wet;
		void hc.devices.history(id, 'water_detected', { limit: 1000 }).then((r) => (spell = r.ok ? lastWet(r.data) : null));
		void hc
			.fetch('/automations')
			.then((r) => (r.ok ? r.json() : []))
			.then((body) => (autos = leakAutomations(Array.isArray(body) ? body : (body.rules ?? []), [id])))
			.catch(() => (autos = []));
	});

	const wetLine = $derived(
		wet && spell?.from
			? `Wet since ${clock(spell.from)}`
			: spell?.from
				? `Last wet ${wetWhen(spell.from)}${spell.to ? ` · ${lasted(spell.from, spell.to)}` : ''}`
				: 'Not wet since homeCore has known it'
	);
	// "today, 7:40 PM", "Sep 2, 7:40 PM".
	const wetWhen = (iso) => {
		const w = when(iso);
		return w === clock(iso) ? `today, ${w}` : `${w}, ${clock(iso)}`;
	};
	const fault = $derived(a.probe_fault === true ? 'Probe fault' : a.freeze_warning === true ? 'Freezing' : '');
</script>

<div class="view">
	<div class="hero" class:wet class:off>
		<span class="words">
			<span class="state">{off ? 'Not reporting' : wet ? 'Wet' : 'Dry'}</span>
			<span class="hc-quiet">{off ? `Last heard ${a.last_seen ? ago(a.last_seen) : 'before homeCore started'} — it isn’t shown as dry while it’s quiet.` : wetLine}</span>
		</span>
	</div>
	<div class="tiles">
		<div class="tile" class:warn={off}>
			<span class="hc-label">Checked in</span>
			<span class="v" class:bad={off}>{a.last_seen ? ago(a.last_seen) : '—'}</span>
			{#if quietAfter(status)}<span class="hc-quiet small">offline after {quietAfter(status)} quiet</span>{/if}
		</div>
		<div class="tile">
			<span class="hc-label">Its temperature</span>
			<span class="v">{a.temperature !== undefined ? (void unitsTick, temp(hc, a.temperature)) : '—'}</span>
			{#if a.freeze_warning === true}<span class="hc-quiet small bad">near freezing</span>{/if}
		</div>
		<div class="tile" class:warn={!!fault || batteryLow(dev, alerts)}>
			<span class="hc-label">Sensor</span>
			<span class="v" class:bad={!!fault}>{fault || 'Working'}</span>
			<span class="hc-quiet small">{fault ? 'it may not notice water' : batteryLow(dev, alerts) ? 'battery low — change it soon' : 'no probe or freeze fault'}</span>
		</div>
	</div>
	<div class="note">
		<strong>If it gets wet</strong>
		<span class="hc-quiet"
			>The bell rings, it shows on the Water card{autos.length ? `, and ${listWords(autos.map((x) => `the automation “${x.name}”`))} ${autos.length === 1 ? 'runs' : 'run'}` : ''}. If it stops checking in it shows as “Not reporting” — never as dry.</span
		>
	</div>
</div>

<style>
	.view {
		display: flex;
		flex-direction: column;
		gap: 18px;
	}
	.hero {
		display: flex;
		align-items: center;
		gap: 14px;
		padding: 18px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.hero.wet {
		background: color-mix(in srgb, var(--alert-bg, #fbe9e2) 70%, var(--surface));
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.state {
		font-size: 28px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.wet .state,
	.off .state {
		color: var(--alert);
	}
	.words .hc-quiet {
		font-size: 14px;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 10px;
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 12px;
		border-radius: 12px;
		background: var(--surface);
		border: 1px solid var(--card-line);
		min-width: 0;
	}
	.tile.warn {
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
	}
	.v {
		font-weight: 600;
	}
	.v.bad,
	.bad {
		color: var(--alert);
	}
	.small {
		font-size: 12px;
	}
	.note {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 14px 16px;
		border-radius: 14px;
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
	}
	.note strong {
		font-weight: 600;
	}
	.note .hc-quiet {
		font-size: 14px;
	}
	@media (max-width: 480px) {
		.tiles {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
