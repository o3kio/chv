<script lang="ts">
	interface Props {
		topologyBox: { x: number; y: number; width: number; height: number };
		displayNodes: any[];
		displayVms: any[];
		pan: { x: number; y: number };
		zoom: number;
		onSelect: (event: MouseEvent) => void;
	}

	let { topologyBox, displayNodes, displayVms, pan, zoom, onSelect }: Props = $props();
</script>

<button type="button" class="topology-minimap" aria-label="Use topology minimap" onclick={onSelect}>
	<svg viewBox={`${topologyBox.x} ${topologyBox.y} ${topologyBox.width} ${topologyBox.height}`}>
		{#each displayNodes as node}
			<rect x={node.x - 10} y={node.y - 8} width="20" height="16" rx="2" class="minimap-node" />
		{/each}
		{#each displayVms as vm}
			<circle cx={vm.x} cy={vm.y} r="3" class="minimap-vm" />
		{/each}
		<rect
			x={topologyBox.x + pan.x + (topologyBox.width - topologyBox.width / zoom) / 2}
			y={topologyBox.y + pan.y + (topologyBox.height - topologyBox.height / zoom) / 2}
			width={topologyBox.width / zoom}
			height={topologyBox.height / zoom}
			class="minimap-view"
		/>
	</svg>
</button>

<style>
	.topology-minimap {
		position: absolute;
		right: 0.75rem;
		bottom: 0.75rem;
		width: 8.75rem;
		height: 6.25rem;
		padding: 0;
		border: 1px solid var(--shell-line-strong);
		border-radius: var(--radius-xs);
		background: color-mix(in srgb, var(--shell-surface) 92%, transparent);
		box-shadow: var(--chv-shadow-sm);
		cursor: crosshair;
		overflow: hidden;
	}

	.topology-minimap svg {
		width: 100%;
		height: 100%;
	}

	.minimap-node {
		fill: var(--color-primary-light);
		stroke: var(--color-primary);
		stroke-width: 2;
	}

	.minimap-vm {
		fill: var(--color-success);
		opacity: 0.85;
	}

	.minimap-view {
		fill: rgba(143, 90, 42, 0.08);
		stroke: var(--color-primary);
		stroke-width: 3;
	}
</style>
