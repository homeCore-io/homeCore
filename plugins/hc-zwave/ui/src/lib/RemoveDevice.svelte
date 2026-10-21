<!-- Remove a device by its pairing button. The device that leaves the network
     leaves homeCore too; automations that used it are turned off, as homeCore
     does when a device is deleted. -->
<script>
	import { cancel, follow, listWords, start, turnOffAutomationsUsing } from './zwave.js';

	let { hc, onclose } = $props();
	const WINDOW = 60;

	let dialog = $state();
	let rid = $state(null);
	let stop = null;
	let left = $state(WINDOW);
	let error = $state('');
	let removed = $state(null); // { name, automations }
	// Names as homeCore knew them, from before the device left.
	let known = {};

	$effect(() => {
		dialog?.showModal();
		void begin();
		const t = setInterval(() => {
			if (rid && left > 0) left -= 1;
		}, 1000);
		return () => {
			clearInterval(t);
			stop?.();
		};
	});

	async function begin() {
		error = '';
		removed = null;
		left = WINDOW;
		known = Object.fromEntries(hc.devices.list().map((d) => [d.device_id, hc.devices.label(d)]));
		const r = await start(hc, 'exclude_node');
		if (r.error) {
			error = r.error;
			return;
		}
		rid = r.rid;
		stop = follow(hc, r.rid, onEvent);
	}

	async function onEvent(e) {
		if (e.stage === 'complete') {
			rid = null;
			const id = e.data?.device_id;
			const name = known[id] ?? e.data?.name ?? (id ? `Node ${id.replace('zwave_', '')}` : 'the device');
			removed = { name, automations: null, other: !known[id] };
			removed = { ...removed, automations: id ? await turnOffAutomationsUsing(hc, id) : [] };
		} else if (e.stage === 'error' || e.stage === 'timeout') {
			rid = null;
			error = e.message ?? 'It stopped';
		} else if (e.stage === 'canceled') {
			rid = null;
		}
	}

	async function close() {
		if (rid) await cancel(hc, rid);
		stop?.();
		dialog?.close();
		onclose?.();
	}
	const mmss = (s) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
</script>

<dialog bind:this={dialog} class="sheet" onclose={() => onclose?.()} aria-label="Remove a device">
	<div class="body">
		<div class="title">
			<span class="kicker">Remove a device</span>
			<button class="x" aria-label="Close" onclick={close}>×</button>
		</div>
		<div class="card">
			<span class="headline">Press the pairing button on the device to remove</span>
			<span class="lead">The same button you used to add it. Any Z-Wave device can be removed this way, even one from another system.</span>
			{#if rid}
				<div class="wait"><span class="dot"></span><span>Waiting… {mmss(left)} left</span></div>
			{/if}
			{#if removed}
				<div class="note">
					<strong>Removed {removed.name}</strong>
					{#if removed.automations === null}
						<span class="hc-quiet">Checking its automations…</span>
					{:else}
						{@const off = removed.automations.off}
						{@const on = removed.automations.on}
						{#if off.length}
							<span class="hc-quiet">{off.length} automation{off.length === 1 ? '' : 's'} used it and {off.length === 1 ? 'is' : 'are'} now turned off: {listWords(off)}.</span>
						{/if}
						{#if on.length}
							<span class="hc-quiet">Still on, and missing it: {listWords(on)} — open {on.length === 1 ? 'it' : 'them'} to choose another device.</span>
						{/if}
						{#if !off.length && !on.length}
							<span class="hc-quiet">{removed.other ? 'It wasn’t one of this house’s devices — it is reset now, ready to add.' : 'No automation used it.'}</span>
						{/if}
					{/if}
				</div>
			{/if}
			{#if error}<span class="err" role="alert">{error}</span>{/if}
			<div class="buttons">
				{#if removed || error}
					<button class="hc-pill big" onclick={begin}>{removed ? 'Remove another' : 'Try again'}</button>
					<button class="hc-pill go big" onclick={close}>Done</button>
				{:else}
					<button class="hc-pill big" onclick={close}>Stop</button>
				{/if}
			</div>
		</div>
		<div class="card soft">
			<strong>A device that’s broken or gone</strong>
			<span class="hc-quiet">If it can’t press a button any more, open it from the page: once it’s “Not answering”, its Health tab offers “Remove it from the network” — or “Replace it” to put a new one in its place and keep its automations.</span>
		</div>
	</div>
</dialog>

<style>
	.sheet {
		border: none;
		padding: 0;
		border-radius: 18px;
		background: var(--bg, var(--surface));
		color: var(--ink);
		width: min(560px, calc(100vw - 32px));
		max-height: calc(100vh - 48px);
		box-shadow: 0 8px 30px rgba(0, 0, 0, 0.18);
	}
	.sheet::backdrop {
		background: var(--scrim, rgba(0, 0, 0, 0.35));
	}
	.body {
		display: flex;
		flex-direction: column;
		gap: 14px;
		padding: 18px;
	}
	.title {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.kicker {
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--ink-muted);
	}
	.x {
		width: 40px;
		height: 40px;
		border-radius: 50%;
		border: none;
		background: transparent;
		font-size: 22px;
		color: var(--ink);
	}
	.card {
		display: flex;
		flex-direction: column;
		gap: 14px;
		padding: 22px;
		border-radius: 18px;
		background: var(--surface);
		border: 1px solid var(--card-line);
	}
	.card.soft {
		gap: 6px;
		padding: 18px 22px;
	}
	.headline {
		font-size: 20px;
		font-weight: 600;
	}
	.lead {
		font-size: 15px;
		color: var(--ink-muted);
	}
	.wait {
		display: flex;
		align-items: center;
		gap: 10px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 70%, var(--surface));
		border-radius: 12px;
		padding: 12px 14px;
	}
	.dot {
		width: 10px;
		height: 10px;
		border-radius: 50%;
		background: var(--track-on, #c98a1a);
		animation: pulse 1.6s ease-in-out infinite;
	}
	@keyframes pulse {
		50% {
			opacity: 0.35;
		}
	}
	.note {
		display: flex;
		flex-direction: column;
		gap: 4px;
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
		border-radius: 12px;
		padding: 12px 14px;
	}
	.note .hc-quiet {
		font-size: 14px;
	}
	.err {
		color: var(--alert);
	}
	.buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		justify-content: flex-end;
	}
	.hc-pill.big {
		min-height: 44px;
		padding: 0 16px;
	}
</style>
