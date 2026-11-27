import type { NodeWithResources, VM } from '#lib/api/types.ts';
import type { OverviewResponse } from '#lib/bff/types.ts';
import type { ShellTone } from '#lib/shell/app-shell.ts';
import { getTaskStatusMeta } from '#lib/webui/tasks.ts';

// ------------------------------------------------------------------
// Overview model types
// ------------------------------------------------------------------

export type OverviewAlert = {
	summary: string;
	scope: string;
	severity: 'critical' | 'warning' | 'info';
};

export type OverviewTask = {
	task_id: string;
	status: string;
	summary: string;
	resource_kind: string;
	resource_id: string;
	operation: string;
	started_unix_ms: number;
	/** #502: the journaled terminal-failure cause — null until a failure records it. */
	error_code?: string | null;
	error_message?: string | null;
};

export type OverviewModel = {
	clusters_total: number;
	clusters_healthy: number;
	clusters_degraded: number;
	nodes_total: number;
	nodes_degraded: number;
	vms_running: number;
	vms_total: number;
	active_tasks: number;
	unresolved_alerts: number;
	maintenance_nodes: number;
	capacity_hotspots: number;
	/** Absent when the control plane does not report fleet utilisation. */
	cpu_usage_percent?: number;
	memory_usage_percent?: number;
	storage_usage_percent?: number;
	alerts: OverviewAlert[];
	recent_tasks: OverviewTask[];
	state: 'ready' | 'loading' | 'empty' | 'error';
};

const EMPTY_OVERVIEW: Omit<OverviewModel, 'state'> = {
	clusters_total: 0,
	clusters_healthy: 0,
	clusters_degraded: 0,
	nodes_total: 0,
	nodes_degraded: 0,
	vms_running: 0,
	vms_total: 0,
	active_tasks: 0,
	unresolved_alerts: 0,
	maintenance_nodes: 0,
	capacity_hotspots: 0,
	alerts: [],
	recent_tasks: []
};

export function createOverview(state: OverviewModel['state']): OverviewModel {
	return { ...EMPTY_OVERVIEW, state };
}

export function toOverviewModel(res: OverviewResponse): OverviewModel {
	const model: OverviewModel = {
		clusters_total: res.clusters_total ?? 0,
		clusters_healthy: res.clusters_healthy ?? 0,
		clusters_degraded: res.clusters_degraded ?? 0,
		nodes_total: res.nodes_total ?? 0,
		nodes_degraded: res.nodes_degraded ?? 0,
		vms_running: res.vms_running ?? 0,
		vms_total: res.vms_total ?? 0,
		active_tasks: res.active_tasks ?? 0,
		unresolved_alerts: res.unresolved_alerts ?? 0,
		maintenance_nodes: res.maintenance_nodes ?? 0,
		capacity_hotspots: res.capacity_hotspots ?? 0,
		cpu_usage_percent: res.cpu_usage_percent,
		memory_usage_percent: res.memory_usage_percent,
		storage_usage_percent: res.storage_usage_percent,
		alerts: (res.alerts ?? []).map((alert) => ({
			summary: alert.summary,
			scope: alert.scope,
			severity: alert.severity as OverviewAlert['severity']
		})),
		recent_tasks: (res.recent_tasks ?? []).map((task) => ({
			task_id: task.task_id,
			status: task.status,
			summary: task.summary,
			resource_kind: task.resource_kind,
			resource_id: task.resource_id,
			operation: task.operation,
			started_unix_ms: task.started_unix_ms,
			error_code: task.error_code ?? null,
			error_message: task.error_message ?? null
		})),
		state: 'ready'
	};

	const hasData =
		model.clusters_total > 0 ||
		model.nodes_total > 0 ||
		model.vms_total > 0 ||
		model.active_tasks > 0 ||
		model.unresolved_alerts > 0 ||
		model.alerts.length > 0 ||
		model.recent_tasks.length > 0;

	if (!hasData) {
		model.state = 'empty';
	}

	return model;
}

// ------------------------------------------------------------------
// Panel helpers
// ------------------------------------------------------------------

export type PanelId = 'briefing' | 'attention' | 'pipeline' | 'capacity';

export const dashboardPanelStorageKey = 'chv.fleet-overview.panels.v1';

export const defaultPanelState: Record<PanelId, boolean> = {
	briefing: true,
	attention: true,
	pipeline: true,
	capacity: true
};

export const dashboardPanels: { id: PanelId; label: string }[] = [
	{ id: 'briefing', label: 'Fleet Briefing' },
	{ id: 'attention', label: 'Immediate Attention' },
	{ id: 'pipeline', label: 'Operation Pipeline' },
	{ id: 'capacity', label: 'Capacity Pressure' }
];

// ------------------------------------------------------------------
// Display helpers
// ------------------------------------------------------------------

export function getPressureState(value: number): string {
	if (value >= 90) return 'Critical';
	if (value >= 75) return 'Pressure';
	if (value >= 45) return 'Warm';
	return 'Idle';
}

export interface RecentTaskViewModel {
	task_id: string;
	operation: string;
	summary: string;
	status: string;
	resource_kind: string;
	resource_id: string;
	started_at: string;
	tone: ShellTone;
	/** #502: the journaled terminal-failure cause — null until a failure records it. */
	error_code?: string | null;
	error_message?: string | null;
}

export function formatRecentTasks(tasks: OverviewTask[]): RecentTaskViewModel[] {
	return tasks.map((t) => {
		const meta = getTaskStatusMeta(t.status);
		return {
			task_id: t.task_id,
			operation: t.operation,
			summary: t.summary,
			status: t.status,
			resource_kind: t.resource_kind,
			resource_id: t.resource_id,
			started_at: new Date(t.started_unix_ms).toLocaleTimeString([], {
				hour: '2-digit',
				minute: '2-digit'
			}),
			tone: meta.tone as ShellTone,
			// #502: the journaled terminal-failure cause, passed through
			// for TaskTimeline's cause rendering (never fabricated).
			error_code: t.error_code ?? null,
			error_message: t.error_message ?? null
		};
	});
}

export function getActiveTopologyResourceIds(tasks: RecentTaskViewModel[]): string[] {
	return tasks
		.filter((task) =>
			['queued', 'running', 'pending', 'accepted', 'in_progress'].includes(task.status.toLowerCase())
		)
		.map((task) => task.resource_id)
		.filter(Boolean);
}

export interface FleetBriefingItem {
	label: string;
	value: string;
	note: string;
}

export function buildFleetBriefing(
	overview: OverviewModel,
	nodes: NodeWithResources[],
	vms: VM[]
): FleetBriefingItem[] {
	return [
		{
			label: 'Control-plane reach',
			value: `${nodes.length} reporting nodes`,
			note:
				overview.nodes_degraded > 0
					? `${overview.nodes_degraded} node signals need review`
					: 'Fleet reporting is stable'
		},
		{
			label: 'Workload posture',
			value: `${overview.vms_running} active of ${overview.vms_total || vms.length}`,
			note:
				overview.unresolved_alerts > 0
					? `${overview.unresolved_alerts} unresolved operator alerts`
					: 'No blocking workload alarms'
		},
		{
			label: 'Execution queue',
			value: `${overview.active_tasks || 0} active operations`,
			note:
				overview.recent_tasks.length > 0
					? `Latest activity: ${overview.recent_tasks[0].operation}`
					: 'No recent task churn'
		}
	];
}

export interface PressureCard {
	label: string;
	value: string;
	width: number;
	state: string;
}

export function buildPressureCards(overview: OverviewModel): PressureCard[] {
	const cards: PressureCard[] = [];

	const addCard = (label: string, value: number | undefined) => {
		// The control plane does not always report utilisation; omit those
		// cards instead of rendering a fabricated 0% / Idle signal.
		if (typeof value !== 'number' || Number.isNaN(value)) return;
		cards.push({
			label,
			value: `${Math.round(value)}%`,
			width: Math.max(0, Math.min(100, value)),
			state: getPressureState(value)
		});
	};

	addCard('CPU envelope', overview.cpu_usage_percent);
	addCard('Memory envelope', overview.memory_usage_percent);
	addCard('Storage pressure', overview.storage_usage_percent);

	return cards;
}
