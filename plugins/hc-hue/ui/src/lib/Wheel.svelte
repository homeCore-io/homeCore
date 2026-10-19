<!-- A colour wheel: the hue around, the saturation outwards. Drag to look,
     let go to send. -->
<script>
	import { hexToHs, hsToHex } from './hue.js';

	let { value = '#ff8a3d', onpick, size = 220 } = $props();
	let dragging = $state(null);
	const shown = $derived(dragging ?? value);
	const at = $derived.by(() => {
		const { h, s } = hexToHs(shown);
		const r = (s * size) / 2;
		const t = ((h - 90) * Math.PI) / 180;
		return { x: size / 2 + r * Math.cos(t), y: size / 2 + r * Math.sin(t) };
	});

	function pick(e) {
		const box = e.currentTarget.getBoundingClientRect();
		const dx = e.clientX - box.left - box.width / 2;
		const dy = e.clientY - box.top - box.height / 2;
		const s = Math.min(1, Math.hypot(dx, dy) / (box.width / 2));
		const h = ((Math.atan2(dy, dx) * 180) / Math.PI + 90 + 360) % 360;
		return hsToHex(h, s);
	}
</script>

<div
	class="wheel"
	style:width={`${size}px`}
	style:height={`${size}px`}
	role="slider"
	tabindex="0"
	aria-label="Colour"
	aria-valuemin="0"
	aria-valuemax="360"
	aria-valuenow={Math.round(hexToHs(shown).h)}
	aria-valuetext={shown}
	onpointerdown={(e) => {
		e.currentTarget.setPointerCapture(e.pointerId);
		dragging = pick(e);
	}}
	onpointermove={(e) => {
		if (dragging) dragging = pick(e);
	}}
	onpointerup={() => {
		if (dragging) onpick?.(dragging);
		dragging = null;
	}}
>
	<span class="mark" style:left={`${at.x}px`} style:top={`${at.y}px`} style:background={shown}></span>
</div>

<style>
	.wheel {
		position: relative;
		flex: none;
		border-radius: 50%;
		touch-action: none;
		cursor: crosshair;
		background:
			radial-gradient(circle, #fff 0%, rgba(255, 255, 255, 0) 70%),
			conic-gradient(#ff0000, #ffff00, #00ff00, #00ffff, #0000ff, #ff00ff, #ff0000);
	}
	.mark {
		position: absolute;
		width: 26px;
		height: 26px;
		margin: -13px 0 0 -13px;
		border-radius: 50%;
		border: 3px solid #fff;
		box-shadow: 0 1px 4px rgba(0, 0, 0, 0.45);
		pointer-events: none;
	}
</style>
