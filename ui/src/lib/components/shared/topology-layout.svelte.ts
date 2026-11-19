import { inventory } from '#lib/stores/inventory.svelte.ts';

function getVmNodeId(vm: { node_id?: string }): string {
	return vm.node_id ?? 'unassigned';
}

const _displayNodes = $derived(
	inventory.nodes.map((n, i) => {
		const columns = Math.max(1, Math.ceil(Math.sqrt(Math.max(inventory.nodes.length, 1))));
		const hostVms = inventory.vms.filter((vm) => getVmNodeId(vm) === n.id);
		return {
			...n,
			x: 190 + (i % columns) * 260,
			y: 245 + Math.floor(i / columns) * 230,
			status: n.status === 'online' ? 'healthy' : n.status === 'error' ? 'danger' : 'warning',
			vmCount: hostVms.length
		};
	})
);

const _displayVms = $derived(
	(() => {
		const siblingIndex = new Map<string, number>();
		const siblingCount = new Map<string, number>();
		for (const vm of inventory.vms) {
			const nodeId = getVmNodeId(vm);
			siblingCount.set(nodeId, (siblingCount.get(nodeId) ?? 0) + 1);
		}

		return inventory.vms.map((v) => {
			const nodeId = getVmNodeId(v);
			const parent = _displayNodes.find((n) => n.id === nodeId);
			const index = siblingIndex.get(nodeId) ?? 0;
			siblingIndex.set(nodeId, index + 1);
			const total = siblingCount.get(nodeId) ?? 1;
			const spread = Math.min(320, Math.max(120, total * 82));
			const offset = total === 1 ? 0 : -spread / 2 + (spread / Math.max(total - 1, 1)) * index;
			const isRunning = v.actual_state === 'running';
			return {
				...v,
				x: parent ? parent.x + offset : 120 + index * 100,
				y: parent ? parent.y - 110 : 95,
				status: isRunning ? 'healthy' : v.actual_state === 'failed' ? 'danger' : 'warning',
				nodeId,
				stateLabel: isRunning ? 'Running' : v.actual_state || 'Unknown'
			};
		});
	})()
);

const _topologyBox = $derived(
	(() => {
		const points = [..._displayNodes, ..._displayVms];
		if (points.length === 0) return { x: 0, y: 0, width: 800, height: 600 };
		const xs = points.map((p: any) => p.x);
		const ys = points.map((p: any) => p.y);
		const x = Math.min(...xs) - 170;
		const y = Math.min(...ys) - 120;
		const width = Math.max(800, Math.max(...xs) - x + 170);
		const height = Math.max(600, Math.max(...ys) - y + 140);
		return { x, y, width, height };
	})()
);

const _showMinimap = $derived(inventory.nodes.length + inventory.vms.length > 12);

export function getDisplayNodes() { return _displayNodes; }
export function getDisplayVms() { return _displayVms; }
export function getTopologyBox() { return _topologyBox; }
export function getShowMinimap() { return _showMinimap; }

export function getStatusColor(status: string) {
	switch (status) {
		case 'healthy': return 'var(--color-success)';
		case 'warning': return 'var(--color-warning)';
		case 'danger': return 'var(--color-danger)';
		default: return 'var(--chv-color-neutral-400)';
	}
}
