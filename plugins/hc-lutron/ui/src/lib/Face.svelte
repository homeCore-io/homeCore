<!-- A seeTouch keypad drawn as it is on the wall: its buttons with their
     engravings and lights, and the lower/raise rocker under them. A tap
     picks a button; the screen around it says what that does. -->
<script>
	import { faceOf, unnamed } from './lutron.js';

	/** `led(n)`: the light's state 0–3, or undefined when it has none.
	 * `lights`: false to draw every light dark (a step's keypad). */
	let { buttons = [], led = () => undefined, selected, onpick, label = 'Keypad', lights = true, disabled = false } = $props();
	const face = $derived(faceOf(buttons));
	const stateOf = (n) => (lights ? led(n) : led(n) === undefined ? undefined : 0);
</script>

<div class="face" role="group" aria-label={label}>
	{#each face.keys as b (b.number)}
		{@const s = stateOf(b.number)}
		<button
			class="key"
			class:sel={selected === b.number}
			aria-pressed={selected === b.number}
			aria-label={`Button ${b.number}, ${b.name}`}
			{disabled}
			onclick={() => onpick?.(b.number)}
		>
			{#if s !== undefined}<span class="led s{s}"></span>{:else}<span class="led none"></span>{/if}
			<span class="label" class:blank={unnamed(b)}>{b.name}</span>
		</button>
	{/each}
	{#if face.lower || face.raise}
		<div class="rocker">
			{#if face.lower}
				<button class:sel={selected === face.lower.number} aria-pressed={selected === face.lower.number} aria-label={face.lower.name} {disabled} onclick={() => onpick?.(face.lower.number)}>
					<svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"
						><path d="M4 6l4 4 4-4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg
					>
				</button>
			{/if}
			{#if face.raise}
				<button class:sel={selected === face.raise.number} aria-pressed={selected === face.raise.number} aria-label={face.raise.name} {disabled} onclick={() => onpick?.(face.raise.number)}>
					<svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"
						><path d="M4 10l4-4 4 4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg
					>
				</button>
			{/if}
		</div>
	{/if}
</div>

<style>
	.face {
		width: 170px;
		flex: none;
		padding: 18px 14px;
		border-radius: 14px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 60%, var(--surface));
		border: 1px solid var(--line);
		box-shadow: inset 0 1px 0 color-mix(in srgb, var(--surface) 70%, transparent);
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	button {
		min-height: 44px;
		border-radius: 8px;
		border: 1px solid color-mix(in srgb, var(--line) 80%, var(--ink) 8%);
		background: var(--sheet, var(--surface));
		color: var(--ink);
	}
	button.sel {
		border: 2px solid var(--accent);
	}
	button:disabled {
		cursor: default;
	}
	.key {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 0 10px;
	}
	.label {
		flex: 1;
		min-width: 0;
		text-align: center;
		font-size: 12px;
		letter-spacing: 0.03em;
		text-transform: uppercase;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.label.blank {
		color: color-mix(in srgb, var(--ink-muted) 75%, var(--surface));
	}
	.led {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--track-off);
	}
	.led.none {
		background: none;
	}
	.led.s1,
	.led.s2,
	.led.s3 {
		background: var(--track-on);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--track-on) 30%, transparent);
	}
	.led.s2 {
		animation: blink 1s steps(1) infinite;
	}
	.led.s3 {
		animation: blink 0.2s steps(1) infinite;
	}
	@keyframes blink {
		50% {
			background: color-mix(in srgb, var(--track-on) 35%, var(--track-off));
			box-shadow: none;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.led.s2,
		.led.s3 {
			animation: none;
		}
	}
	.rocker {
		display: flex;
		gap: 6px;
		margin-top: 6px;
	}
	.rocker button {
		flex: 1;
		color: var(--ink-muted);
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
</style>
