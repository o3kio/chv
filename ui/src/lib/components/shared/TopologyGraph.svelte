<script lang="ts">
	import { Box, MoreHorizontal, Server } from 'lucide-svelte';
	import { draw } from 'svelte/transition';
	import { selection } from '#lib/stores/selection.svelte.ts';
	import { getStatusColor } from './topology-layout.svelte';

	interface Props {
		viewBox: string;
		displayNodes: any[];
		displayVms: any[];
		highlightedIds: Set<string>;
		onContextMenuNode?: (event: MouseEvent, node: any) => void;
		onContextMenuVm?: (event: MouseEvent, vm: any) => void;
	}

	let { viewBox, displayNodes, displayVms, highlightedIds, onContextMenuNode, onContextMenuVm }: Props = $props();

	function handleKeydown(event: KeyboardEvent, target: 'node' | 'vm', resource: any) {
		if (event.key === 'Enter' || event.key === ' ') {
			event.preventDefault();
			if (target === 'node') {
				selection.select('node', resource.id, resource.name);
			} else {
				selection.select('vm', resource.id, resource.name);
			}
		}
	}
</script>

<svg {viewBox} class="canvas-svg">
	<!-- Connections -->
	{#each displayVms as vm}
		{@const parent = displayNodes.find((n) => n.id === vm.nodeId)}
		{#if parent}
			<path
				d="M {vm.x} {vm.y + 22} C {vm.x} {vm.y + 70}, {parent.x} {parent.y - 70}, {parent.x} {parent.y - 32}"
				class="connection-line"
				class:connection-line--active={highlightedIds.has(vm.id) || highlightedIds.has(parent.id)}
				style:--status-color={getStatusColor(vm.status)}
				in:draw={{ duration: 1000 }}
			/>
		{/if}
	{/each}

	<!-- Nodes (Hypervisors) -->
	{#each displayNodes as node}
		<g
			class="node-group"
			class:node-group--active={selection.active.id === node.id}
			class:node-group--highlighted={highlightedIds.has(node.id)}
			role="button"
			tabindex="0"
			aria-label="Host {node.name}, {node.vmCount} workloads"
			onclick={() => selection.select('node', node.id, node.name)}
			onkeydown={(e) => handleKeydown(e, 'node', node)}
			oncontextmenu={(e) => { e.preventDefault(); e.stopPropagation(); onContextMenuNode?.(e, node); }}
		>
			<rect
				x={node.x - 74} y={node.y - 34}
				width="148" height="68"
				rx="6"
				class="node-box"
				style:--status-color={getStatusColor(node.status)}
			/>
			<Server x={node.x - 58} y={node.y - 11} size={18} color={getStatusColor(node.status)} />
			<text x={node.x - 28} y={node.y - 5} class="node-label">
				{node.name}
			</text>
			<text x={node.x - 28} y={node.y + 14} class="node-meta">
				{node.vmCount} workloads
			</text>
			<circle cx={node.x + 55} cy={node.y - 20} r="4" class="status-indicator" style:fill={getStatusColor(node.status)} />
			<MoreHorizontal x={node.x + 46} y={node.y + 10} size={16} color="var(--chv-color-neutral-500)" />
		</g>
	{/each}

	<!-- VMs -->
	{#each displayVms as vm}
		<g
			class="vm-group"
			class:vm-group--active={selection.active.id === vm.id}
			class:vm-group--highlighted={highlightedIds.has(vm.id)}
			role="button"
			tabindex="0"
			aria-label="Instance {vm.name}, {vm.stateLabel}"
			onclick={() => selection.select('vm', vm.id, vm.name)}
			onkeydown={(e) => handleKeydown(e, 'vm', vm)}
			oncontextmenu={(e) => { e.preventDefault(); e.stopPropagation(); onContextMenuVm?.(e, vm); }}
		>
			<rect
				x={vm.x - 58} y={vm.y - 24}
				width="116" height="48"
				rx="5"
				class="vm-box"
				style:--status-color={getStatusColor(vm.status)}
			/>
			<Box x={vm.x - 45} y={vm.y - 9} size={15} color={getStatusColor(vm.status)} />
			<text x={vm.x - 22} y={vm.y - 2} class="vm-label">
				{vm.name}
			</text>
			<text x={vm.x - 22} y={vm.y + 14} class="vm-meta">
				{vm.stateLabel}
			</text>
			<circle cx={vm.x + 43} cy={vm.y - 12} r="3" class="status-indicator" style:fill={getStatusColor(vm.status)} />
		</g>
	{/each}
</svg>

<style>
	.canvas-svg {
		width: 100%;
		height: 100%;
		transition: opacity 180ms var(--ease-default);
	}

	.connection-line {
		stroke: var(--status-color, var(--chv-color-neutral-200));
		stroke-width: 1.25;
		fill: none;
		opacity: 0.48;
		stroke-dasharray: 6 7;
		animation: dash 20s linear infinite;
	}

	.connection-line--active {
		stroke: var(--color-primary);
		stroke-width: 2.5;
		opacity: 0.9;
		stroke-dasharray: 10 5;
		animation-duration: 6s;
	}

	@keyframes dash {
		to { stroke-dashoffset: 100; }
	}

	.node-box {
		fill: var(--bg-surface);
		stroke: color-mix(in srgb, var(--status-color) 36%, var(--border-subtle));
		stroke-width: 1;
		filter: drop-shadow(0 1px 1px rgba(0, 0, 0, 0.04));
		transition:
			fill 180ms var(--ease-default),
			stroke 180ms var(--ease-default),
			stroke-width 180ms var(--ease-default);
	}

	.node-group {
		cursor: pointer;
	}

	.node-group:hover .node-box {
		stroke: var(--color-primary);
		stroke-width: 2;
	}

	.node-group--highlighted .node-box,
	.vm-group--highlighted .vm-box {
		stroke: var(--color-primary);
		stroke-width: 2;
		filter: drop-shadow(0 0 10px rgba(143, 90, 42, 0.22));
	}

	.node-group--active .node-box {
		stroke: var(--color-primary);
		stroke-width: 2;
		fill: var(--color-primary-light);
	}

	.node-label {
		font-size: 12px;
		font-weight: 600;
		fill: var(--chv-color-neutral-800);
	}

	.node-meta,
	.vm-meta {
		font-size: 9px;
		font-weight: 500;
		fill: var(--chv-color-neutral-500);
	}

	.vm-box {
		fill: var(--bg-surface);
		stroke: color-mix(in srgb, var(--status-color) 32%, var(--border-subtle));
		stroke-width: 1;
		transition:
			fill 180ms var(--ease-default),
			stroke 180ms var(--ease-default),
			stroke-width 180ms var(--ease-default);
	}

	.vm-group {
		cursor: pointer;
	}

	.vm-group:hover .vm-box {
		stroke: var(--color-primary);
		stroke-width: 2;
	}

	.vm-group--active .vm-box {
		fill: var(--color-accent-soft);
		stroke: var(--color-accent);
		stroke-width: 2;
	}

	.vm-label {
		font-size: 10px;
		font-weight: 600;
		fill: var(--chv-color-neutral-700);
	}

	.status-indicator {
		filter: drop-shadow(0 0 5px color-mix(in srgb, currentColor 35%, transparent));
	}

	@media (prefers-reduced-motion: reduce) {
		.connection-line {
			animation: none;
		}

		.canvas-svg,
		.node-box,
		.vm-box {
			transition: none;
		}
	}
</style>
