<!-- What the lock reported, newest first: locked and unlocked and how, what
     it couldn't do, and the door opening. -->
<script>
	import { when } from './yolink.js';

	let { history = null, tick = Date.now() } = $props();
</script>

<div class="history">
	<span class="hc-quiet lead">What the lock reported since homeCore started, newest first. Automations can start on any of these, e.g. “when unlocked by fingerprint”.</span>
	{#if history === null}
		<p class="hc-quiet">Reading its history…</p>
	{:else if !history.length}
		<p class="hc-quiet">Nothing yet. Each lock and unlock it reports shows here.</p>
	{:else}
		<div class="list">
			{#each history.slice(0, 60) as h (`${h.at}|${h.what}`)}
				<div class="row">
					<span class="at hc-num">{when(h.at, tick)}</span>
					<span class="what" class:bad={h.bad}>{h.what}</span>
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
		width: 76px;
		font-size: 12px;
		color: var(--ink-muted);
	}
	.what {
		flex: 1;
		font-size: 15px;
		min-width: 0;
	}
	.what.bad {
		color: var(--alert);
		font-weight: 600;
	}
	.how {
		font-size: 13px;
		color: var(--ink-muted);
		text-align: right;
	}
</style>
