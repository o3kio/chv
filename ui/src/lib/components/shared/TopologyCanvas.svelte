<script lang="ts">
	import { goto } from '$app/navigation';
	import { inventory } from '#lib/stores/inventory.svelte.ts';
	import { selection } from '#lib/stores/selection.svelte.ts';
	import { Loader2 } from 'lucide-svelte';
	import {
		getDisplayNodes,
		getDisplayVms,
		getTopologyBox,
		getShowMinimap,
		getStatusColor
	} from './topology-layout.svelte';
	import { getSelectedResource } from './topology-selection.svelte';
	import TopologyHeader from './TopologyHeader.svelte';
	import TopologyGraph from './TopologyGraph.svelte';
	import TopologyMinimap from './TopologyMinimap.svelte';
	import TopologySelectionPanel from './TopologySelectionPanel.svelte';
	import TopologyContextMenu from './TopologyContextMenu.svelte';
	import TopologyFooter from './TopologyFooter.svelte';
	type TopologyTarget = 'fleet' | 'node' | 'vm';

	interface MenuAction {
		label: string;
		hint?: string;
		dangerous?: boolean;
		disabled?: boolean;
		run: () => void;
	}

	interface Props {
		highlightedResourceIds?: string[];
	}

	let { highlightedResourceIds = [] }: Props = $props();

	let zoom = $state(1);
	let pan = $state({ x: 0, y: 0 });
	let isPanning = $state(false);
	let dragStart = $state<{ x: number; y: number; panX: number; panY: number } | null>(null);
	let contextMenu = $state<{
		x: number;
		y: number;
		title: string;
		subtitle: string;
		type: TopologyTarget;
		actions: MenuAction[];
	} | null>(null);
	let canvasElement = $state<HTMLDivElement | null>(null);

	const highlightedIds = $derived(new Set(highlightedResourceIds));
	const displayNodes = $derived(getDisplayNodes());
	const displayVms = $derived(getDisplayVms());
	const topologyBox = $derived(getTopologyBox());
	const showMinimap = $derived(getShowMinimap());
	const selectedResource = $derived(getSelectedResource());

	const viewBox = $derived(
		(() => {
			const width = topologyBox.width / zoom;
			const height = topologyBox.height / zoom;
			const x = topologyBox.x + pan.x + (topologyBox.width - width) / 2;
			const y = topologyBox.y + pan.y + (topologyBox.height - height) / 2;
			return `${x} ${y} ${width} ${height}`;
		})()
	);

	function setZoom(next: number) {
		zoom = Math.min(1.8, Math.max(0.65, next));
	}

	function fitFleet() {
		zoom = 1;
		pan = { x: 0, y: 0 };
	}

	function fitSelection() {
		const id = selection.active.id;
		if (!id) {
			fitFleet();
			return;
		}
		const point = [...displayNodes, ...displayVms].find((item) => item.id === id);
		if (!point) {
			fitFleet();
			return;
		}
		zoom = 1.25;
		pan = {
			x: point.x - topologyBox.x - topologyBox.width / 2,
			y: point.y - topologyBox.y - topologyBox.height / 2
		};
	}

	function handlePointerDown(event: PointerEvent) {
		if (event.button !== 0 || contextMenu) return;
		const target = event.target as Element;
		if (
			target.closest('.node-group') ||
			target.closest('.vm-group') ||
			target.closest('.topology-menu') ||
			target.closest('.topology-minimap') ||
			target.closest('.selection-panel')
		) return;
		isPanning = true;
		dragStart = { x: event.clientX, y: event.clientY, panX: pan.x, panY: pan.y };
		canvasElement?.setPointerCapture(event.pointerId);
	}

	function handlePointerMove(event: PointerEvent) {
		if (!isPanning || !dragStart || !canvasElement) return;
		const rect = canvasElement.getBoundingClientRect();
		const viewWidth = topologyBox.width / zoom;
		const viewHeight = topologyBox.height / zoom;
		const dx = ((event.clientX - dragStart.x) / Math.max(rect.width, 1)) * viewWidth;
		const dy = ((event.clientY - dragStart.y) / Math.max(rect.height, 1)) * viewHeight;
		pan = {
			x: dragStart.panX - dx,
			y: dragStart.panY - dy
		};
	}

	function handlePointerUp(event: PointerEvent) {
		isPanning = false;
		dragStart = null;
		if (canvasElement?.hasPointerCapture(event.pointerId)) {
			canvasElement.releasePointerCapture(event.pointerId);
		}
	}

	function openContextMenu(event: MouseEvent, target: TopologyTarget, resource?: any) {
		event.preventDefault();
		event.stopPropagation();

		if (target === 'node' && resource) {
			contextMenu = {
				x: event.clientX,
				y: event.clientY,
				title: resource.name,
				subtitle: `${resource.vmCount} workloads · ${resource.status}`,
				type: target,
				actions: [
					{ label: 'Open host', hint: 'Details', run: () => goto(`/nodes/${resource.id}`) },
					{ label: 'Show instances', hint: 'Filtered list', run: () => goto(`/vms?node_id=${resource.id}`) },
					{ label: 'Show networks', hint: 'Host scope', run: () => goto(`/networks?node_id=${resource.id}`) },
					{ label: 'Show storage', hint: 'Host scope', run: () => goto(`/volumes?node_id=${resource.id}`) }
				]
			};
		} else if (target === 'vm' && resource) {
			contextMenu = {
				x: event.clientX,
				y: event.clientY,
				title: resource.name,
				subtitle: `${resource.stateLabel} · ${resource.nodeId}`,
				type: target,
				actions: [
					{ label: 'Open instance', hint: 'Details', run: () => goto(`/vms/${resource.id}`) },
					{ label: 'Open console', hint: 'Serial', disabled: resource.status !== 'healthy', run: () => goto(`/vms/${resource.id}?tab=console`) },
					{ label: 'View metrics', hint: 'Performance', run: () => goto(`/vms/${resource.id}?tab=metrics`) },
					{ label: 'Review events', hint: 'Audit trail', run: () => goto(`/events?resource_id=${resource.id}`) }
				]
			};
		} else {
			contextMenu = {
				x: event.clientX,
				y: event.clientY,
				title: 'Global Fleet',
				subtitle: `${inventory.nodes.length} hosts · ${inventory.vms.length} workloads`,
				type: 'fleet',
				actions: [
					{ label: 'Open instances', hint: 'All workloads', run: () => goto('/vms') },
					{ label: 'Open tasks', hint: 'Operations', run: () => goto('/tasks') },
					{ label: 'Open events', hint: 'Fleet log', run: () => goto('/events') },
					{ label: 'Refresh topology', hint: 'Fetch latest', run: () => inventory.fetch() }
				]
			};
		}
	}

	function selectFromMinimap(event: MouseEvent) {
		if (!canvasElement) return;
		const rect = (event.currentTarget as SVGSVGElement).getBoundingClientRect();
		const x = topologyBox.x + ((event.clientX - rect.left) / Math.max(rect.width, 1)) * topologyBox.width;
		const y = topologyBox.y + ((event.clientY - rect.top) / Math.max(rect.height, 1)) * topologyBox.height;
		pan = {
			x: x - topologyBox.x - topologyBox.width / 2,
			y: y - topologyBox.y - topologyBox.height / 2
		};
	}
</script>

<div class="topology-canvas">
	<TopologyHeader {zoom} onZoom={setZoom} onFitFleet={fitFleet} onFitSelection={fitSelection} />

	<div
		bind:this={canvasElement}
		class="svg-container"
		class:svg-container--panning={isPanning}
		role="application"
		aria-label="Interactive live fleet topology"
		onpointerdown={handlePointerDown}
		onpointermove={handlePointerMove}
		onpointerup={handlePointerUp}
		onpointercancel={handlePointerUp}
		oncontextmenu={(e) => { e.preventDefault(); openContextMenu(e, 'fleet'); }}
	>
		{#if inventory.isLoading}
			<div class="canvas-loading">
				<Loader2 size={24} class="animate-spin" />
				<span>Fetching technical topology...</span>
			</div>
		{:else}
			<TopologyGraph
				{viewBox}
				{displayNodes}
				{displayVms}
				{highlightedIds}
				onContextMenuNode={(e, node) => openContextMenu(e, 'node', node)}
				onContextMenuVm={(e, vm) => openContextMenu(e, 'vm', vm)}
			/>

			{#if showMinimap}
				<TopologyMinimap
					{topologyBox}
					{displayNodes}
					{displayVms}
					{pan}
					{zoom}
					onSelect={selectFromMinimap}
				/>
			{/if}

			{#if selectedResource}
				<TopologySelectionPanel resource={selectedResource} {getStatusColor} />
			{/if}
		{/if}

		<TopologyContextMenu {contextMenu} onClose={() => contextMenu = null} />
	</div>

	<TopologyFooter />
</div>

<style>
	.topology-canvas {
		background: var(--bg-surface);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-sm);
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 500px;
		box-shadow: var(--chv-shadow-sm);
		overflow: hidden;
	}

	.svg-container {
		flex: 1;
		background-color: var(--bg-base);
		background-image:
			radial-gradient(var(--dot-grid) 1px, transparent 0),
			radial-gradient(var(--dot-grid) 1px, transparent 0);
		background-position: 0 0, 10px 10px;
		background-size: 20px 20px;
		overflow: hidden;
		position: relative;
		cursor: grab;
		touch-action: none;
	}

	.svg-container--panning {
		cursor: grabbing;
	}

	.canvas-loading {
		position: absolute;
		inset: 0;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 1rem;
		color: var(--chv-color-neutral-400);
		font-size: 11px;
	}

	:global(.animate-spin) {
		animation: spin 1s linear infinite;
	}

	@keyframes spin {
		from { transform: rotate(0deg); }
		to { transform: rotate(360deg); }
	}
	@media (prefers-reduced-motion: reduce) {
		:global(.animate-spin) {
			animation: none;
		}
	}
</style>
