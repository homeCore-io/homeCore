<!--
	A WLED controller's playlists: presets it steps through on its own — it
	keeps going if homeCore is down. Each step's preset, how long it stays and
	its fade; how many times it plays, what happens at the end, shuffled or
	not. A playlist is kept in a preset slot of its own, so automations start
	it like any preset.
-->
<script lang="ts">
	import type { CoreDevice } from '#lib/devices.js';
	import { commands } from '#lib/commands.svelte.js';
	import { ask } from '#lib/confirm.svelte.js';
	import { presetsOf, type PlaylistView, type Preset } from '#lib/wled.js';

	let { device, disabled }: { device: CoreDevice; disabled: boolean } = $props();

	const id = $derived(device.device_id);
	const all = $derived(presetsOf(device));
	const playlists = $derived(all.filter((p) => p.kind === 'playlist'));
	const looks = $derived(all.filter((p) => p.kind !== 'playlist'));
	const playing = $derived(device.attributes.playlist_id);

	let openId = $state<number | 'new' | undefined>(undefined);
	const open = $derived(
		openId === 'new' ? undefined : (playlists.find((p) => p.id === openId) ?? playlists[0])
	);

	let name = $state('');
	let draft = $state<PlaylistView>({ steps: [], repeat: 0, shuffle: false, end_preset_id: 0 });
	let editing = $state<number | 'new' | undefined>(undefined);

	function edit(p: Preset | undefined) {
		if (p?.playlist) {
			name = p.name;
			draft = structuredClone($state.snapshot(p.playlist));
			editing = p.id;
		} else {
			name = 'New playlist';
			draft = {
				steps: looks
					.slice(0, 2)
					.map((l) => ({ preset_id: l.id, duration_s: 600, transition_ms: 700 })),
				repeat: 0,
				shuffle: false,
				end_preset_id: 0
			};
			editing = 'new';
		}
	}
	$effect(() => {
		if (editing === undefined) {
			if (openId === 'new') edit(undefined);
			else if (open) edit(open);
		}
	});

	const nameOf = (pid: number) => all.find((p) => p.id === pid)?.name ?? `Preset ${pid}`;
	function save(start = false) {
		void commands.act(id, 'save_playlist', {
			...(typeof editing === 'number' ? { slot: editing } : {}),
			name: name.trim() || 'Playlist',
			playlist: $state.snapshot(draft),
			start
		});
	}
	async function remove(p: Preset) {
		if (!(await ask({ title: `Delete the playlist “${p.name}”?`, go: 'Delete', stay: 'Keep it' })))
			return;
		void commands.act(id, 'delete_preset', { delete_preset: p.id });
		openId = undefined;
		editing = undefined;
	}
	const minutes = (s: number) => (s >= 60 && s % 60 === 0 ? `${s / 60} min` : `${s} s`);
	const seconds = (text: string) => {
		const m = /^\s*([\d.]+)\s*(m|min|minutes?)?\s*$/i.exec(text);
		if (!m) return undefined;
		return m[2] ? Number(m[1]) * 60 : Number(m[1]);
	};
</script>

<div class="playlists">
	<div class="cols">
		<ul class="list">
			{#each playlists as p (p.id)}
				<li>
					<button class:on={editing === p.id} onclick={() => ((openId = p.id), edit(p))}>
						<b>{p.name}</b>
						<span class="num"
							>slot {p.id} · {p.playlist?.steps.length ?? 0} presets · {p.playlist?.repeat
								? `${p.playlist.repeat} times`
								: 'forever'}{p.id === playing ? ' · playing' : ''}</span
						>
					</button>
				</li>
			{/each}
			{#if playlists.length === 0}<li class="quiet">No playlists yet.</li>{/if}
			<li>
				<button class="new" {disabled} onclick={() => ((openId = 'new'), edit(undefined))}
					>New playlist</button
				>
			</li>
		</ul>

		{#if editing !== undefined}
			<section class="editor">
				<label>
					<span class="label">Name</span>
					<input bind:value={name} maxlength="32" {disabled} />
				</label>
				<div class="steps">
					<div class="step head label">
						<span>Preset</span><span>Stays for</span><span>Fade in</span><span></span>
					</div>
					{#each draft.steps as s, i (i)}
						<div class="step">
							<select bind:value={s.preset_id} aria-label="Preset" {disabled}>
								{#each looks as l (l.id)}<option value={l.id}>{l.id} · {l.name}</option>{/each}
								{#if !looks.some((l) => l.id === s.preset_id)}<option value={s.preset_id}
										>{nameOf(s.preset_id)}</option
									>{/if}
							</select>
							<input
								aria-label="Stays for"
								value={minutes(s.duration_s)}
								{disabled}
								onchange={(e) => {
									const v = seconds(e.currentTarget.value);
									if (v !== undefined) s.duration_s = v;
								}}
							/>
							<input
								aria-label="Fade in (seconds)"
								value={`${s.transition_ms / 1000} s`}
								{disabled}
								onchange={(e) => {
									const v = seconds(e.currentTarget.value);
									if (v !== undefined) s.transition_ms = Math.round(v * 1000);
								}}
							/>
							<button
								class="x"
								aria-label="Remove this step"
								{disabled}
								onclick={() => draft.steps.splice(i, 1)}>×</button
							>
						</div>
					{/each}
					<button
						class="pill"
						disabled={disabled || looks.length === 0}
						onclick={() =>
							draft.steps.push({ preset_id: looks[0]!.id, duration_s: 600, transition_ms: 700 })}
						>Add a preset</button
					>
				</div>
				<div class="opts">
					<label>
						<span class="label">Play it</span>
						<select bind:value={draft.repeat} {disabled}>
							<option value={0}>Forever</option>
							{#each [1, 2, 3, 5, 10] as n (n)}<option value={n}
									>{n === 1 ? 'Once' : `${n} times`}</option
								>{/each}
						</select>
					</label>
					<label>
						<span class="label">When it ends</span>
						<select bind:value={draft.end_preset_id} {disabled}>
							<option value={0}>Stay on the last preset</option>
							<option value={255}>Go back to what ran before</option>
							{#each looks as l (l.id)}<option value={l.id}>Apply {l.name}</option>{/each}
						</select>
					</label>
					<label class="check"
						><input type="checkbox" bind:checked={draft.shuffle} {disabled} /> Shuffle the order</label
					>
				</div>
				<div class="row-buttons">
					<button
						class="primary"
						disabled={disabled || draft.steps.length === 0}
						onclick={() => save(false)}>Save</button
					>
					<button
						class="pill"
						disabled={disabled || draft.steps.length === 0}
						onclick={() => save(true)}>Save and play</button
					>
					{#if typeof editing === 'number'}
						{@const p = playlists.find((x) => x.id === editing)}
						<button
							class="pill"
							{disabled}
							onclick={() => commands.act(id, 'start_playlist', { playlist_id: editing })}
							>Play it now</button
						>
						{#if editing === playing}<button
								class="pill"
								{disabled}
								onclick={() => commands.act(id, 'next_in_playlist')}>Next</button
							>{/if}
						{#if p}<button class="pill danger" {disabled} onclick={() => remove(p)}>Delete</button
							>{/if}
					{/if}
				</div>
			</section>
		{/if}
	</div>
</div>

<style>
	.playlists {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.cols {
		display: flex;
		flex-wrap: wrap;
		gap: 16px;
		align-items: flex-start;
	}
	.list {
		flex: 1 1 220px;
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.list button {
		font: inherit;
		width: 100%;
		text-align: left;
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 10px 12px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.list button.on {
		border-color: var(--accent);
		background: var(--lit-bg);
	}
	.list .new {
		color: var(--accent);
		font-weight: 600;
	}
	.editor {
		flex: 3 1 360px;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 14px;
		padding: 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius);
		background: var(--surface);
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	input,
	select {
		font: inherit;
		min-height: 38px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 8px;
		min-width: 0;
	}
	.steps {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.step {
		display: grid;
		grid-template-columns: minmax(0, 3fr) minmax(0, 1fr) minmax(0, 1fr) 40px;
		gap: 8px;
		align-items: center;
	}
	.x {
		font: inherit;
		min-height: 38px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.opts {
		display: flex;
		flex-wrap: wrap;
		gap: 12px 24px;
		align-items: flex-end;
		border-top: 1px solid var(--card-line);
		padding-top: 12px;
	}
	.check {
		flex-direction: row;
		align-items: center;
		gap: 8px;
		min-height: var(--target);
	}
	.check input {
		width: 18px;
		height: 18px;
		min-height: 0;
		accent-color: var(--accent);
	}
	.label {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
	}
	.num {
		font-family: var(--font-num);
		font-size: 12px;
		color: var(--ink-muted);
	}
	.quiet {
		color: var(--ink-muted);
		font-size: 14px;
		padding: 4px 2px;
	}
	.row-buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.primary {
		font: inherit;
		font-weight: 600;
		min-height: 40px;
		padding: 0 18px;
		border-radius: var(--radius-chip);
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
	}
	.pill {
		font: inherit;
		font-size: 14px;
		min-height: 40px;
		padding: 0 14px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		align-self: flex-start;
	}
	.pill.danger {
		color: var(--alert);
		border-color: var(--alert-line);
	}
	button:disabled {
		opacity: 0.55;
	}
</style>
