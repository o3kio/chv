import type { GetMaintenanceResponse } from '#lib/bff/types.ts';

export type MaintenanceWindowView = {
	window_id: string;
	name: string;
	status: string;
	startsAt: string;
	endsAt: string;
};

export type MaintenanceNodeView = {
	node_id: string;
	name: string;
	state: string;
};

export type MaintenanceView = {
	windows: MaintenanceWindowView[];
	nodes: MaintenanceNodeView[];
	pendingActions: number;
	empty: boolean;
};

/**
 * Format a BFF timestamp without inventing one. An absent/invalid value is
 * returned verbatim (or as an em dash) rather than "now" or a relative time.
 */
export function formatMaintenanceTimestamp(value: string | null | undefined): string {
	if (!value) return '—';
	const parsed = new Date(value);
	if (Number.isNaN(parsed.getTime())) return value;
	return parsed.toLocaleString();
}

/**
 * Map the maintenance read contract to the view model. The BFF reports window
 * names and node states; there is no upgrade/orchestrator/posture payload, so
 * this model deliberately has no fields for them.
 */
export function buildMaintenanceView(res: GetMaintenanceResponse): MaintenanceView {
	const windows = (res.windows ?? []).map((window) => ({
		window_id: window.window_id,
		name: window.name,
		status: window.status,
		startsAt: formatMaintenanceTimestamp(window.start_time),
		endsAt: formatMaintenanceTimestamp(window.end_time)
	}));
	const nodes = (res.nodes ?? []).map((node) => ({
		node_id: node.node_id,
		name: node.name,
		state: node.state
	}));
	const pendingActions = res.pending_actions ?? 0;

	return {
		windows,
		nodes,
		pendingActions,
		empty: windows.length === 0 && nodes.length === 0 && pendingActions === 0
	};
}
