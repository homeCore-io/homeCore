<!-- A lock: locked or not, and how it last changed; Lock / Unlock (unlocking
     asks first); the door, the battery, the bolt; and how it locks again by
     itself. -->
<script>
	import { failed, lastLine, lowBattery, num, RELOCK_CHOICES, relockBody, relockOf } from './zwave.js';

	let { hc, dev, names = {}, canRun = false, admin = false } = $props();

	const a = $derived(dev?.attributes ?? {});
	const label = $derived(dev ? hc.devices.label(dev) : '');
	const state = $derived(a.jammed === true ? 'Jammed' : a.locked === true ? 'Locked' : a.locked === false ? 'Unlocked' : 'Not sure');
	const line = $derived(lastLine(dev, names));
	const battery = $derived(num(a.battery_pct));
	let alerts = $state();
	$effect(() => hc?.alerts?.subscribe((list) => (alerts = list)));
	const low = $derived(lowBattery(undefined, dev, alerts));
	const relock = $derived(relockOf(dev));
	const canRelock = $derived(a.lock_operation_type !== undefined || a.lock_auto_relock_secs !== undefined);
	const choices = $derived(RELOCK_CHOICES.some(([s]) => s === relock) ? RELOCK_CHOICES : [...RELOCK_CHOICES, [relock, `${relock} seconds`]]);

	let error = $state('');
	let busy = $state(false);
	// A command homeCore took can still fail at the lock; the plugin says why
	// on the device (`last_command_error`).
	let sentAt = $state(0);
	let lastSaid = $state(undefined);
	$effect(() => {
		const said = a.last_command_error ?? null;
		if (lastSaid !== undefined && said && said !== lastSaid && Date.now() - sentAt < 30000) error = said;
		lastSaid = said;
	});

	async function send(body) {
		error = '';
		sentAt = Date.now();
		busy = true;
		const r = await hc.command(dev.device_id, body);
		busy = false;
		if (!r.ok) error = r.error;
	}

	async function toggle() {
		// Unknown offers Lock: securing it is the safe thing to offer.
		if (a.locked === true) {
			const ok = await hc.ui.confirm({ title: `Unlock ${label}?`, body: 'The door opens to anyone.', go: 'Unlock' });
			if (!ok) return;
			await send({ locked: false });
		} else await send({ locked: true });
	}
</script>

<div class="lock">
	<div class="hero">
		<div class="words">
			<span class="state" class:open={a.locked !== true}>{state}</span>
			{#if line}<span class="hc-quiet">{line}</span>{/if}
		</div>
		<button class="go" disabled={!canRun || busy || dev?.available === false} onclick={toggle}>{a.locked === true ? 'Unlock' : 'Lock'}</button>
	</div>
	{#if error}<span class="err" role="alert">{error}</span>{/if}
	<div class="tiles">
		<div class="tile" class:warn={a.open === true}>
			<span class="hc-label">Door</span>
			<span class="v">{a.open === true ? 'Open' : a.open === false ? 'Closed' : '—'}</span>
		</div>
		<div class="tile" class:warn={low}>
			<span class="hc-label">Battery</span>
			<span class="v" class:bad={low}>{battery === undefined ? '—' : `${battery}%`}{low ? ' · change soon' : ''}</span>
		</div>
		<div class="tile" class:warn={a.jammed === true}>
			<span class="hc-label">Bolt</span>
			<span class="v" class:bad={a.jammed === true}>{a.jammed === true ? 'Jammed' : 'Free · not jammed'}</span>
		</div>
	</div>
	{#if canRelock}
		<div class="card">
			<span class="t">Locking by itself</span>
			<label class="row"
				><span>Lock again after</span><select
					value={String(relock)}
					disabled={!admin || busy || dev?.available === false}
					onchange={(e) => send(relockBody(dev, Number(e.currentTarget.value)))}
				>
					{#each choices as [secs, words] (secs)}<option value={String(secs)}>{words}</option>{/each}
				</select></label
			>
			<span class="hc-quiet">The lock does this itself, so it still works if homeCore is down.</span>
		</div>
	{/if}
</div>

<style>
	.lock {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	.hero {
		display: flex;
		align-items: center;
		gap: 18px;
		padding: 18px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 4px;
		flex: 1;
		min-width: 0;
	}
	.state {
		font-size: 28px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.state.open {
		color: var(--alert);
	}
	.words .hc-quiet {
		font-size: 14px;
	}
	.go {
		min-height: 52px;
		padding: 0 26px;
		border-radius: 14px;
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
		font-weight: 600;
		font-size: 16px;
	}
	.go:disabled {
		opacity: 0.5;
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
		border-color: color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
	}
	.v {
		font-weight: 600;
	}
	.v.bad {
		color: var(--alert);
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.t {
		font-weight: 600;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		min-height: 40px;
	}
	select {
		min-height: 40px;
		min-width: 150px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 10px;
	}
	.card .hc-quiet {
		font-size: 13px;
	}
	.err {
		color: var(--alert);
		font-size: 14px;
	}
	@media (max-width: 420px) {
		.hero {
			flex-wrap: wrap;
		}
		.go {
			width: 100%;
		}
	}
</style>
