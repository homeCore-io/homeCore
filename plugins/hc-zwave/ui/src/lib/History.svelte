<!-- What the lock reported, newest first: unlocked with which code, locked by
     itself, by hand, from an automation, a wrong code, a jam. -->
<script>
	import { when } from './zwave.js';

	let { history = null, tick = Date.now() } = $props();
</script>

<div class="history">
	<span class="hc-quiet lead">What the lock reported, newest first. Automations can start on any of these, e.g. “when unlocked with code 2”.</span>
	{#if history === null}
		<p class="hc-quiet">Reading its history…</p>
	{:else if !history.length}
		<p class="hc-quiet">Nothing yet. Each lock and unlock it reports shows here.</p>
	{:else}
		<div class="list">
			{#each history.slice(0, 50) as h (`${h.at}|${h.what}`)}
				<div class="row">
					<span class="at hc-num">{when(h.at, tick)}</span>
					<span class="what">{h.what}</span>
					{#if h.how}<span class="how">{h.how}</span>{/if}
				</div>
			{/each}
		</div>
	{/if}
</div>

<style>
	.history {
		display: flex;
		flex-direction: column;
		gap: 10px;
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
		align-items: center;
		gap: 14px;
		min-height: 52px;
		padding: 6px 16px;
		border-top: 1px solid var(--card-line);
	}
	.row:first-child {
		border-top: none;
	}
	.at {
		flex: none;
		width: 70px;
		font-size: 12px;
		color: var(--ink-muted);
	}
	.what {
		flex: 1;
		font-size: 15px;
		min-width: 0;
	}
	.how {
		font-size: 13px;
		color: var(--ink-muted);
		text-align: right;
	}
</style>
