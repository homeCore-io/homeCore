<!-- A YoLink lock: locked or not, and how it last changed; Lock / Unlock
     (unlocking asks first); the door, the battery, the last problem; and the
     lock's own settings — lock again after, beep volume. -->
<script>
	import { autoChoices, batteryLow, BATTERY_WORDS, lastLine, lastProblem, lockState, num, quarters, SOUND } from './yolink.js';

	let { hc, dev, canRun = false } = $props();

	const a = $derived(dev?.attributes ?? {});
	const label = $derived(dev ? hc.devices.label(dev) : '');
	const off = $derived(dev?.available === false);
	const line = $derived(lastLine(dev));
	const q = $derived(quarters(dev));
	const low = $derived(batteryLow(dev));
	const problem = $derived(lastProblem(dev));
	const auto = $derived(num(a.auto_lock_secs));
	const sound = $derived(num(a.sound_level));
	const settable = $derived(auto !== undefined || sound !== undefined);

	let error = $state('');
	let busy = $state(false);
	// A command homeCore took can still fail at the hub; the plugin says why
	// on the device (`last_command_error`).
	let sentAt = $state(0);
	let lastSaid = $state(undefined);
	$effect(() => {
		const said = a.last_command_error ?? null;
		if (lastSaid !== undefined && said && said !== lastSaid && Date.now() - sentAt < 45000) error = said;
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
			<span class="state" class:open={a.locked !== true}>{off ? 'Not reporting' : lockState(dev)}</span>
			{#if line}<span class="hc-quiet">{line}</span>{/if}
		</div>
		<button class="go" disabled={!canRun || busy || off} onclick={toggle}>{a.locked === true ? 'Unlock' : 'Lock'}</button>
	</div>
	{#if error}<span class="err" role="alert">{error}</span>{/if}
	<div class="tiles">
		<div class="tile" class:warn={a.open === true}>
			<span class="hc-label">Door</span>
			<span class="v">{a.open === true ? 'Open' : a.open === false ? 'Closed' : 'No door sensor'}</span>
		</div>
		<div class="tile" class:warn={low}>
			<span class="hc-label">Battery</span>
			<span class="v" class:bad={low}>{q === undefined ? '—' : low ? `Low · ${q} of 4` : `${BATTERY_WORDS[q]} · ${q} of 4`}</span>
		</div>
		<div class="tile">
			<span class="hc-label">Last problem</span>
			{#if problem}
				<span class="v">{problem.what}</span>
				<span class="hc-quiet small">{problem.line}</span>
			{:else}
				<span class="v">None</span>
			{/if}
		</div>
	</div>
	{#if settable}
		<div class="card">
			<span class="t">The lock’s own settings</span>
			{#if auto !== undefined}
				<label class="row"
					><span>Lock again after</span><select value={String(auto)} disabled={!canRun || busy || off} onchange={(e) => send({ auto_lock_secs: Number(e.currentTarget.value) })}>
						{#each autoChoices(auto) as [secs, words] (secs)}<option value={String(secs)}>{words}</option>{/each}
					</select></label
				>
			{/if}
			{#if sound !== undefined}
				<label class="row"
					><span>Beep volume</span><select value={String(sound)} disabled={!canRun || busy || off} onchange={(e) => send({ sound_level: Number(e.currentTarget.value) })}>
						{#each SOUND as [level, words] (level)}<option value={String(level)}>{words}</option>{/each}
					</select></label
				>
			{/if}
			<span class="hc-quiet small">The lock keeps these itself, so they work if homeCore is down. Codes and fingerprints stay in the YoLink app.</span>
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
	.small {
		font-size: 12px;
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
	.card .small {
		font-size: 13px;
	}
	.err {
		color: var(--alert);
		font-size: 14px;
	}
	@media (max-width: 480px) {
		.hero {
			flex-wrap: wrap;
		}
		.go {
			width: 100%;
		}
		.tiles {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
