<!-- A room's scenes as tiles of their colours; the running one is marked,
     and a tap applies it. -->
<script>
	import { paletteOf } from './hue.js';

	let { hc, scenes, columns = 2, compact = false, where = false, options = {}, onerror } = $props();
	let busy = $state('');

	async function apply(scene) {
		busy = scene.device_id;
		const r = await hc.command(scene.device_id, { action: 'activate', ...options });
		busy = '';
		if (!r.ok) onerror?.(r.error);
	}
</script>

<div class="tiles" class:compact style:grid-template-columns={`repeat(${columns}, minmax(0, 1fr))`}>
	{#each scenes as s (s.device_id)}
		<button
			class="scene"
			class:on={s.attributes.active === true}
			class:busy={busy === s.device_id}
			aria-pressed={s.attributes.active === true}
			onclick={() => apply(s)}
		>
			<span class="pal">
				{#each paletteOf(s) as c, i (i)}<span style:background={c}></span>{/each}
			</span>
			<b>{hc.devices.label(s)}</b>
			{#if s.attributes.smart}<span class="where">changes through the day</span>{/if}
			{#if where}<span class="where">{s.attributes.group_name}</span>{/if}
		</button>
	{/each}
</div>

<style>
	.tiles {
		display: grid;
		gap: 8px;
	}
	.scene {
		display: flex;
		flex-direction: column;
		gap: 7px;
		padding: 9px;
		border-radius: 12px;
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
		min-width: 0;
	}
	.scene.on {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.scene.busy {
		opacity: 0.6;
	}
	.pal {
		display: flex;
		height: 34px;
		border-radius: 8px;
		overflow: hidden;
	}
	.compact .pal {
		height: 24px;
	}
	.pal span {
		flex: 1;
	}
	b {
		font-size: 14px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.compact b {
		font-size: 12.5px;
	}
	.where {
		font-size: 11px;
		color: var(--ink-muted);
	}
</style>
