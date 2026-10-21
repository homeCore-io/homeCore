<!-- A lock's codes: each slot, who it is for and when it was last used. Codes
     go to the lock and are never shown or kept by homeCore; only admins can
     change them. -->
<script>
	import { failed, slotLine } from './zwave.js';

	let { hc, deviceId, slots = [], names = {}, lastUsed = {}, admin = false, onchanged } = $props();

	let all = $state(false);
	let editing = $state(null); // { slot, person, code, isNew }
	let busy = $state(0);
	let error = $state('');
	let said = $state('');

	const used = $derived(slots.filter((s) => s.status !== 'available'));
	const empty = $derived(slots.filter((s) => s.status === 'available'));
	const shown = $derived(all ? slots : [...used, ...empty.slice(0, 2)].sort((x, y) => x.slot - y.slot));

	async function act(id, params, done) {
		error = '';
		said = '';
		busy = params.slot;
		const r = await hc.action(id, { device_id: deviceId, ...params });
		busy = 0;
		const err = failed(r);
		if (err) error = err;
		else {
			said = done;
			onchanged?.();
		}
		return !err;
	}

	const toggle = (s) => act('set_code_enabled', { slot: s.slot, enabled: s.status === 'disabled' }, s.status === 'disabled' ? `Code ${s.slot} is on.` : `Code ${s.slot} is off — still on the lock.`);

	async function clear(s) {
		const ok = await hc.ui.confirm({
			title: `Clear code ${s.slot}${names[s.slot] ? ` · ${names[s.slot]}` : ''}?`,
			body: 'The code stops opening the lock, and is gone from it.',
			go: 'Clear',
			danger: true
		});
		if (ok) await act('clear_user_code', { slot: s.slot }, `Code ${s.slot} is cleared.`);
	}

	const open = (s) => (editing = { slot: s.slot, person: names[s.slot] ?? '', code: '', isNew: s.status === 'available' });
	const codeOk = $derived(!editing || (editing.isNew ? /^[0-9]{4,8}$/.test(editing.code) : editing.code === '' || /^[0-9]{4,8}$/.test(editing.code)));

	async function sendCode(ev) {
		ev.preventDefault();
		if (!editing || !codeOk) return;
		const params = { slot: editing.slot, person: editing.person.trim() };
		if (editing.code) params.code = editing.code;
		// The code leaves this screen as it goes: not kept here either.
		editing.code = '';
		const ok = await act('set_user_code', params, params.code ? `Code ${params.slot} is on the lock.` : `Code ${params.slot} renamed.`);
		if (ok) editing = null;
	}
</script>

<div class="codes">
	<span class="hc-quiet lead">Codes go to the lock and are never shown or kept by homeCore. Only admins can change them.</span>
	<div class="list">
		{#each shown as s (s.slot)}
			<div class="row">
				<span class="n hc-num">{s.slot}</span>
				<span class="words"
					><span class="name" class:unused={s.status === 'available'}>{s.status === 'available' ? 'Empty' : names[s.slot] ?? `Code ${s.slot}`}</span>{#if slotLine(s, lastUsed[s.slot])}<span class="sub">{slotLine(s, lastUsed[s.slot])}</span>{/if}</span
				>
				{#if admin}
					{#if s.status === 'available'}
						<button class="hc-pill small" disabled={!!busy} onclick={() => open(s)}>Add a code</button>
					{:else}
						<span class="btns">
							<button class="hc-pill small" disabled={!!busy} onclick={() => toggle(s)}>{s.status === 'disabled' ? 'Turn on' : 'Turn off'}</button>
							<button class="hc-pill small" disabled={!!busy} onclick={() => open(s)}>Change</button>
							<button class="hc-pill small danger" disabled={!!busy} onclick={() => clear(s)}>Clear</button>
						</span>
					{/if}
				{/if}
			</div>
		{/each}
	</div>
	{#if slots.length > shown.length || all}
		<button class="more" onclick={() => (all = !all)}>{all ? 'Show fewer' : `Show all ${slots.length} slots`}</button>
	{/if}
	{#if editing}
		<form class="edit" onsubmit={sendCode}>
			<span class="t">Code {editing.slot}</span>
			<div class="two">
				<label class="hc-field"><span class="hc-label">Who it’s for</span><input type="text" placeholder="e.g. Dog walker" bind:value={editing.person} /></label>
				<label class="hc-field"
					><span class="hc-label">{editing.isNew ? 'Code (4–8 digits)' : 'New code (blank keeps it)'}</span><input
						type="password"
						inputmode="numeric"
						autocomplete="off"
						maxlength="8"
						bind:value={editing.code}
					/></label
				>
			</div>
			<div class="end">
				<button type="button" class="hc-pill" onclick={() => (editing = null)}>Cancel</button>
				<button class="hc-pill go" disabled={!codeOk || !!busy}>{editing.code ? 'Send to the lock' : 'Save'}</button>
			</div>
		</form>
	{/if}
	{#if error}<span class="err" role="alert">{error}</span>{/if}
	{#if said}<span class="ok">{said}</span>{/if}
</div>

<style>
	.codes {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.lead {
		font-size: 14px;
	}
	.list {
		display: flex;
		flex-direction: column;
		background: var(--surface);
		border: 1px solid var(--card-line);
		border-radius: 16px;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 12px;
		min-height: 60px;
		padding: 8px 16px;
		border-top: 1px solid var(--card-line);
	}
	.row:first-child {
		border-top: none;
	}
	.n {
		width: 24px;
		font-size: 13px;
		color: var(--ink-muted);
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 1px;
		flex: 1 1 140px;
		min-width: 0;
	}
	.name {
		font-weight: 600;
	}
	.name.unused {
		font-weight: 400;
		color: var(--ink-muted);
	}
	.sub {
		font-size: 12px;
		color: var(--ink-muted);
	}
	.btns {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.hc-pill.small {
		min-height: 36px;
		padding: 0 12px;
	}
	.more {
		align-self: flex-start;
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-weight: 600;
		font-size: 14px;
	}
	.edit {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 16px;
		border-radius: 16px;
		background: var(--surface);
		border: 1px solid color-mix(in srgb, var(--accent) 50%, var(--card-line));
	}
	.t {
		font-weight: 600;
	}
	.two {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
		gap: 10px;
	}
	.two input {
		min-height: 42px;
	}
	.end {
		display: flex;
		gap: 8px;
		justify-content: flex-end;
	}
	.end .hc-pill {
		min-height: 40px;
	}
	.err {
		color: var(--alert);
		font-size: 14px;
	}
	.ok {
		font-size: 14px;
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
</style>
