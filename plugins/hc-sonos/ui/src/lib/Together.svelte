<!-- "Play together": tick the speakers that should play the same music; the
     one marked "playing from" leads. The plugin makes the whole group in one
     go and answers speaker by speaker — joined, left, no answer — and this
     says so on each row, with a way to try the ones that didn't answer. -->
<script>
	import { groupsOf, groupSummary, inputOf, isPlaying, makeGroup, playbackWord, hasMusic, RESULT_WORDS } from './sonos.js';

	let { hc, devices = [], leader: startLeader, ticked = [], onclose } = $props();

	const label = (d) => hc.devices.label(d);
	const byId = (id) => devices.find((d) => d.device_id === id);

	// The order is fixed when the sheet opens: the leader, its group, the rest.
	const initial = () => {
		const lead = startLeader ?? devices.find(isPlaying)?.device_id ?? devices[0]?.device_id;
		const first = [lead, ...ticked.filter((id) => id !== lead)];
		const rest = devices
			.filter((d) => !first.includes(d.device_id))
			.sort((a, b) => label(a).localeCompare(label(b)))
			.map((d) => d.device_id);
		return { lead, order: [...first, ...rest].filter((id) => byId(id)), on: new Set(first.filter((id) => byId(id))) };
	};
	const start = initial();
	let leader = $state(start.lead);
	let on = $state(start.on);
	const order = start.order;

	let busy = $state(false);
	let results = $state(null);
	let error = $state('');
	const resultOf = (id) => results?.find((r) => r.device_id === id);
	const unanswered = $derived((results ?? []).filter((r) => r.result === 'no_answer'));

	const now = (d) => (d.available === false ? 'Not answering' : hasMusic(d) ? `${inputOf(d) || d.attributes.media_title} — ${playbackWord(d)}` : 'Nothing playing');

	function toggle(id) {
		const next = new Set(on);
		if (next.has(id)) {
			next.delete(id);
			if (id === leader) leader = [...next][0] ?? id;
		} else next.add(id);
		if (!next.size) next.add(leader);
		on = next;
		results = null;
	}
	const lead = (id) => {
		leader = id;
		results = null;
	};
	const everywhere = () => {
		on = new Set(order);
		results = null;
	};
	const alone = () => {
		on = new Set([leader]);
		results = null;
	};

	async function apply() {
		busy = true;
		error = '';
		const members = order.filter((id) => on.has(id) && id !== leader);
		let answers;
		if (!members.length) {
			// Each on its own: every group in the house comes apart.
			const leaders = new Set(groupsOf(devices, label).filter((g) => g.grouped).map((g) => g.id));
			leaders.add(leader);
			answers = await Promise.all([...leaders].map((l) => makeGroup(hc, l, [])));
		} else {
			answers = [await makeGroup(hc, leader, members)];
		}
		const failed = answers.find((a) => !a.ok);
		if (failed) error = failed.error;
		results = answers.flatMap((a) => a.results);
		busy = false;
	}
	const applyLabel = $derived(
		busy ? 'Grouping…' : unanswered.length ? `Try ${unanswered.map((r) => r.name).join(' and ')} again` : on.size > 1 ? 'Group them' : 'Play each on its own'
	);

	let sheet = $state();
	$effect(() => sheet?.focus());
	const keys = (e) => e.key === 'Escape' && onclose?.();
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="scrim" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose?.()} onkeydown={keys}>
	<div class="sheet" role="dialog" aria-modal="true" aria-labelledby="together-title" tabindex="-1" bind:this={sheet}>
		<div class="head">
			<div class="words">
				<span class="title" id="together-title">Play together</span>
				<span class="hc-quiet">Tick the speakers that should play the same music. The music comes from the one marked “playing from”.</span>
			</div>
			<button class="close" aria-label="Close" onclick={() => onclose?.()}>×</button>
		</div>

		<div class="rows">
			{#each order as id (id)}
				{@const d = byId(id)}
				{@const r = resultOf(id)}
				<div class="row">
					<label class="pick">
						<input type="checkbox" checked={on.has(id)} onchange={() => toggle(id)} />
						<span class="who"><b>{label(d)}</b><span class="hc-quiet">{now(d)}</span></span>
					</label>
					{#if on.has(id) && id === leader}
						<span class="from">playing from</span>
					{:else if on.has(id)}
						<button class="lead" onclick={() => lead(id)}>Play from here</button>
					{/if}
					<span class="result" class:bad={r && (r.result === 'no_answer' || r.result === 'failed')}>{r ? RESULT_WORDS[r.result] ?? r.result : ''}</span>
				</div>
			{/each}
		</div>

		<div class="actions">
			<button class="hc-pill" onclick={everywhere} disabled={busy}>All speakers</button>
			<button class="hc-pill" onclick={alone} disabled={busy}>Each on its own</button>
			<span class="gap"></span>
			<button class="hc-pill go" onclick={apply} disabled={busy}>{applyLabel}</button>
		</div>

		{#if results}
			<div class="done" role="status">
				<strong>{groupSummary(results)}</strong>
				{#if unanswered.length}<span class="hc-quiet">{unanswered.length === 1 ? 'It may be off or asleep.' : 'They may be off or asleep.'} Try again, or leave {unanswered.length === 1 ? 'it' : 'them'} out.</span>{/if}
			</div>
		{/if}
		{#if error}<p class="err" role="alert">{error}</p>{/if}
	</div>
</div>

<style>
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 50;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 16px;
		background: var(--scrim, rgba(0, 0, 0, 0.35));
	}
	.sheet {
		width: min(560px, 100%);
		max-height: calc(100vh - 32px);
		overflow: auto;
		display: flex;
		flex-direction: column;
		gap: 16px;
		padding: 20px;
		border-radius: var(--radius-sheet, 18px);
		background: var(--surface);
		border: 1px solid var(--card-line);
		box-shadow: 0 8px 30px rgba(0, 0, 0, 0.12);
		color: var(--ink);
		outline: none;
	}
	.head {
		display: flex;
		align-items: flex-start;
		gap: 12px;
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.title {
		font-size: 20px;
		font-weight: 600;
	}
	.words .hc-quiet {
		font-size: 14px;
	}
	.close {
		flex: none;
		width: 36px;
		height: 36px;
		border-radius: 50%;
		border: 1px solid var(--line);
		background: var(--surface);
		font-size: 20px;
		line-height: 1;
		color: var(--ink-muted);
	}
	.rows {
		display: flex;
		flex-direction: column;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 12px;
		min-height: 56px;
		border-top: 1px solid var(--card-line);
	}
	.pick {
		flex: 1;
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
		cursor: pointer;
	}
	.pick input {
		width: 20px;
		height: 20px;
		flex: none;
		accent-color: var(--accent);
	}
	.who {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.who b {
		font-weight: 600;
	}
	.who .hc-quiet {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.from {
		flex: none;
		font-size: 12px;
		font-weight: 600;
		color: var(--lit-ink, var(--accent));
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--track-on) 55%, var(--surface));
		padding: 4px 8px;
		border-radius: 9px;
	}
	.lead {
		flex: none;
		min-height: 36px;
		padding: 0 10px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink-muted);
		font-size: 13px;
	}
	.result {
		flex: none;
		width: 84px;
		text-align: right;
		font-size: 13px;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.result.bad {
		color: var(--alert);
		font-weight: 600;
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.actions .hc-pill {
		min-height: 40px;
	}
	.actions .go {
		min-height: 44px;
		padding: 0 20px;
	}
	.gap {
		flex: 1;
	}
	.done {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 12px 14px;
		border-radius: 12px;
		font-size: 14px;
		background: color-mix(in srgb, var(--lit-bg) 45%, var(--surface));
		border: 1px solid color-mix(in srgb, var(--lit-bg) 90%, var(--ink) 6%);
	}
	.done strong {
		font-weight: 600;
	}
	.done .hc-quiet {
		font-size: 14px;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 14px;
	}
	@media (max-width: 600px) {
		.scrim {
			align-items: flex-end;
			padding: 0;
		}
		.sheet {
			width: 100%;
			border-radius: var(--radius-sheet, 18px) var(--radius-sheet, 18px) 0 0;
			max-height: 92vh;
		}
		.result {
			width: auto;
			min-width: 56px;
		}
	}
</style>
