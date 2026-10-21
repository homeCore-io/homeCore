<!-- A volume: an optional name or speaker mark, the slider, the number. It
     shows what is being dragged until the speaker reports it back, and sends
     as it goes, no faster than four times a second. -->
<script>
	import Icon from './Icon.svelte';
	import { throttle } from './sonos.js';

	let { value = 0, label = '', strong = false, icon = false, disabled = false, min = 0, max = 100, name = 'Volume', onset } = $props();

	let drag = $state(null);
	let clear = null;
	const shown = $derived(drag ?? value);
	const send = throttle((v) => onset?.(v), 250);
	function input(e) {
		clearTimeout(clear);
		drag = Number(e.currentTarget.value);
		send(drag);
	}
	function done(e) {
		const v = Number(e.currentTarget.value);
		onset?.(v);
		clearTimeout(clear);
		clear = setTimeout(() => (drag = null), 1500);
	}
</script>

<div class="vol">
	{#if icon}<span class="mark"><Icon name="speaker" /></span>{/if}
	{#if label}<span class="label" class:strong>{label}</span>{/if}
	<input type="range" {min} {max} step="1" value={shown} {disabled} aria-label={label ? `${label} volume` : name} oninput={input} onchange={done} />
	<span class="n hc-num">{shown}</span>
</div>

<style>
	.vol {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
	}
	.mark {
		color: var(--ink-muted);
	}
	.label {
		flex: none;
		width: 92px;
		font-size: 13px;
		color: var(--ink-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.label.strong {
		font-weight: 600;
		color: var(--ink);
	}
	input {
		flex: 1;
		min-width: 0;
		accent-color: var(--accent);
	}
	.n {
		flex: none;
		width: 28px;
		text-align: right;
		font-size: 13px;
	}
</style>
