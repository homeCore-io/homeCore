<!-- Add a device: Pairing → Security (S2 PIN, only for a device that asks)
     → Getting to know it (the interview) → Name and room. With `replace`, the
     same flow puts a new device in a failed one's place, keeping its id,
     room and automations. -->
<script>
	import { cancel, failed, follow, respond, roomsOf, start } from './zwave.js';

	let { hc, replace = null, replaceName = '', onclose } = $props();

	const STEPS = ['Pairing', 'Security', 'Getting to know it', 'Name'];
	const WINDOW = 240;

	let dialog = $state();
	let at = $state(1); // 1..4
	let rid = $state(null);
	let stop = null;
	let error = $state('');
	let warning = $state('');
	let left = $state(WINDOW);
	let security = $state(null); // x-zwave of the security prompt
	let pin = $state('');
	let strongest = $state(true);
	let interview = $state({ percent: 30, message: '' });
	let naming = $state(null); // x-zwave of the name prompt
	let name = $state('');
	let room = $state('');
	let rooms = $state([]);
	let done = $state(null);
	let busy = $state(false);
	let secureSeen = $state(false);

	$effect(() => {
		dialog?.showModal();
		void begin();
		const t = setInterval(() => {
			if (at === 1 && rid && left > 0) left -= 1;
		}, 1000);
		return () => {
			clearInterval(t);
			stop?.();
		};
	});

	function reset() {
		at = 1;
		rid = null;
		error = '';
		warning = '';
		left = WINDOW;
		security = null;
		pin = '';
		strongest = true;
		interview = { percent: 30, message: '' };
		naming = null;
		name = '';
		room = '';
		done = null;
		secureSeen = false;
	}

	async function begin() {
		reset();
		const r = await start(hc, 'include_node', replace ? { replace_device_id: replace } : {});
		if (r.error) {
			error = r.error;
			return;
		}
		rid = r.rid;
		stop = follow(hc, r.rid, onEvent);
	}

	function onEvent(e) {
		if (e.stage === 'progress') {
			if (e.label === 'interview') {
				at = 3;
				interview = { percent: e.percent ?? interview.percent, message: e.message ?? interview.message };
			} else if (e.label === 'security') {
				at = Math.max(at, 2);
			}
		} else if (e.stage === 'awaiting_user') {
			const x = e.response_schema?.['x-zwave'] ?? {};
			if (x.step === 'security' || x.step === 'pin') {
				security = x;
				secureSeen = true;
				at = 2;
			} else if (x.step === 'name') {
				naming = x;
				name = e.response_schema?.name?.default ?? '';
				at = 4;
				void roomsOf(hc).then((list) => (rooms = list));
			}
		} else if (e.stage === 'warning') {
			warning = e.message ?? '';
		} else if (e.stage === 'complete') {
			done = e.data ?? {};
			rid = null;
		} else if (e.stage === 'error' || e.stage === 'timeout') {
			error = e.message ?? 'It stopped';
			rid = null;
		} else if (e.stage === 'canceled') {
			rid = null;
		}
	}

	async function answer(response) {
		busy = true;
		const r = await respond(hc, rid, response);
		const err = failed(r);
		if (err) error = err;
		busy = false;
	}

	const sendSecurity = (grant) => {
		const resp = { grant };
		if (grant !== 'none' && security?.needs_pin && strongest) resp.pin = pin.trim();
		security = null;
		at = 3;
		interview = { percent: 20, message: grant === 'none' ? 'Adding it without security' : 'Exchanging keys with the device' };
		void answer(resp);
	};
	const pinOk = $derived(!security?.needs_pin || !strongest || /^[0-9]{5}$/.test(pin.trim()));

	async function finish(again) {
		await answer({ name: name.trim(), room });
		if (again) {
			stop?.();
			await begin();
		}
	}

	async function close() {
		// Pairing or security: stop listening. The interview carries on by
		// itself; the device appears when it is done.
		if (rid && at <= 2) await cancel(hc, rid);
		stop?.();
		dialog?.close();
		onclose?.();
	}

	const pill = (i) => (at === i + 1 ? 'on' : at > i + 1 || (i === 1 && at > 2 && !secureSeen) ? 'done' : '');
	const mmss = (s) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
</script>

<dialog bind:this={dialog} class="sheet" onclose={() => onclose?.()} aria-label={replace ? 'Replace a device' : 'Add a device'}>
	<div class="body">
		<div class="title">
			<span class="kicker">{replace ? `Replace ${replaceName || 'a device'}` : 'Add a device'}</span>
			<button class="x" aria-label="Close" onclick={close}>×</button>
		</div>
		<div class="steps">
			{#each STEPS as s, i (s)}
				<span class="step {pill(i)}">{s}</span>
			{/each}
		</div>

		{#if error}
			<div class="part">
				<span class="headline">That didn’t work</span>
				<span class="lead">{error}</span>
				<div class="buttons">
					<button class="hc-pill" onclick={close}>Close</button>
					<button class="hc-pill go" onclick={begin}>Try again</button>
				</div>
			</div>
		{:else if done}
			<div class="part">
				<span class="headline">{replace ? `Replaced ${done.name || replaceName || 'it'}` : done.name ? `Added ${done.name}` : 'Added'}</span>
				<span class="lead">{done.room ? `In ${done.room}. ` : ''}It’s on Home now, and in this list.</span>
				<div class="buttons">
					{#if !replace}<button class="hc-pill" onclick={begin}>Add another</button>{/if}
					<button class="hc-pill go" onclick={close}>Done</button>
				</div>
			</div>
		{:else if at === 1}
			<div class="part">
				<span class="headline">Put the device in pairing mode</span>
				<span class="lead"
					>{replace
						? 'The new device takes the old one’s place: same name, room and automations. Usually a button pressed three times, or the steps in its manual.'
						: 'Usually a button pressed three times, or the device’s own steps in its manual. Keep it near the controller.'}</span
				>
				<div class="wait"><span class="dot"></span><span>{rid ? `Listening for a new device… ${mmss(left)} left` : 'Starting…'}</span></div>
				<div class="buttons"><button class="hc-pill big" onclick={close}>Stop</button></div>
			</div>
		{:else if at === 2 && security}
			<div class="part">
				<span class="headline">Found a secure device</span>
				{#if security.needs_pin}
					<span class="lead">Type the first five digits of the PIN on its label (or the box) — they’re the underlined part. This proves it’s yours, so nobody nearby can slip in.</span>
					<label class="hc-field pin"
						><span class="hc-label">PIN</span><input
							type="text"
							inputmode="numeric"
							maxlength="5"
							placeholder="12345"
							autocomplete="off"
							disabled={!strongest}
							bind:value={pin}
						/></label
					>
				{:else}
					<span class="lead">It asks for {security.requested?.join(', ') || 'secure'} keys, which need no PIN.</span>
				{/if}
				<label class="check"><input type="checkbox" bind:checked={strongest} />Let it use the strongest security it offers</label>
				{#if !strongest}<span class="hc-quiet">It is added with basic security{security.needs_pin ? ', so no PIN is needed' : ''}.</span>{/if}
				<div class="buttons">
					<button class="hc-pill big" disabled={busy} onclick={() => sendSecurity('none')}>Add without security</button>
					<button class="hc-pill go big" disabled={busy || !pinOk} onclick={() => sendSecurity(strongest ? 'strongest' : 'basic')}>Continue</button>
				</div>
			</div>
		{:else if at <= 3}
			<div class="part">
				<span class="headline">Getting to know it</span>
				<span class="lead">Z-Wave asks the device what it can do. A lock or battery device can take a few minutes — you can close this; it finishes on its own.</span>
				<div class="bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={interview.percent}><div style:width={`${interview.percent}%`}></div></div>
				<span class="line">{interview.message || 'Waiting for it to answer'}</span>
				{#if warning}<span class="warn">{warning}</span>{/if}
			</div>
		{:else if at === 4 && naming}
			<div class="part">
				<span class="headline">Name it and put it in a room</span>
				<div class="two">
					<label class="hc-field"><span class="hc-label">Name</span><input type="text" bind:value={name} /></label>
					<label class="hc-field"
						><span class="hc-label">Room</span><select bind:value={room}>
							<option value="">No room</option>
							{#each rooms as r (r)}<option value={r}>{r}</option>{/each}
						</select></label
					>
				</div>
				<span class="hc-quiet">The name is kept in Z-Wave JS too, so both show the same.</span>
				{#if warning}<span class="warn">{warning}</span>{/if}
				<div class="buttons">
					{#if !replace}<button class="hc-pill big" disabled={busy} onclick={() => finish(true)}>Add another</button>{/if}
					<button class="hc-pill go big" disabled={busy} onclick={() => finish(false)}>Done</button>
				</div>
			</div>
		{/if}
		{#if !done && !error && at <= 2}
			<span class="foot hc-quiet">The PIN step appears only for devices with S2 security; older devices skip it.</span>
		{/if}
	</div>
</dialog>

<style>
	.sheet {
		border: none;
		padding: 0;
		border-radius: 18px;
		background: var(--surface);
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
		gap: 18px;
		padding: 22px;
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
	.steps {
		display: flex;
		gap: 6px;
	}
	.step {
		flex: 1;
		min-height: 36px;
		display: flex;
		align-items: center;
		justify-content: center;
		text-align: center;
		border-radius: 9px;
		font-size: 13px;
		font-weight: 600;
		line-height: 1.15;
		padding: 2px 4px;
		border: 1px solid var(--card-line);
		color: var(--ink-muted);
		background: var(--surface);
	}
	.step.on {
		background: color-mix(in srgb, var(--lit-bg) 60%, var(--surface));
		border-color: color-mix(in srgb, var(--accent) 45%, var(--card-line));
		color: var(--lit-ink, var(--accent));
	}
	.step.done {
		background: color-mix(in srgb, var(--bg, #f3f0e8) 70%, var(--surface));
		color: light-dark(#2f5a27, #8fd19e);
	}
	.part {
		display: flex;
		flex-direction: column;
		gap: 12px;
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
	.buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		justify-content: flex-end;
	}
	.big.hc-pill,
	.hc-pill.big {
		min-height: 44px;
		padding: 0 16px;
	}
	.pin input {
		width: 110px;
		min-height: 44px;
		font-family: var(--font-num, monospace);
		font-size: 16px;
		letter-spacing: 0.08em;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 14px;
	}
	.check input {
		width: 18px;
		height: 18px;
	}
	.bar {
		height: 8px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--track-off, #ddd) 60%, var(--surface));
		overflow: hidden;
	}
	.bar div {
		height: 100%;
		background: var(--accent);
		transition: width 0.4s;
	}
	.line {
		font-size: 14px;
	}
	.warn {
		font-size: 14px;
		color: var(--lit-ink, var(--alert));
	}
	.two {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 10px;
	}
	.two input,
	.two select {
		min-height: 44px;
	}
	.foot {
		font-size: 13px;
	}
</style>
