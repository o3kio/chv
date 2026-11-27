import type { PageLoad } from './$types';
import { getStoredToken } from '#lib/api/client.ts';
import { getVm, getVmConsoleUrl } from '#lib/bff/vms.ts';
import { cachedFetch, DETAIL_TTL } from '#lib/stores/api-cache.svelte.ts';
import type { VmSummary, RelatedTask, AttachedVolume, AttachedNic } from '#lib/bff/types.ts';

const VALID_TABS = new Set(['summary', 'console', 'boot-log', 'events', 'metrics', 'snapshots']);

function resolveTab(tab: string | null): string {
	// Removed tabs (volumes, networks, tasks, configuration) still exist in
	// bookmarks; fall back to Summary instead of rendering a blank panel.
	return tab && VALID_TABS.has(tab) ? tab : 'summary';
}

export type VmDetailModel = {
	state: 'ready' | 'empty' | 'error';
	currentTab: string;
	errorMessage?: string;
	summary: {
		vm_id: string;
		name: string;
		node_id: string;
		power_state: string;
		health: string;
		cpu: string;
		memory: string;
		attached_volumes?: AttachedVolume[];
		attached_nics?: AttachedNic[];
		snapshot_count?: number;
	};
	sections: { id: string; label: string; count?: number }[];
	recent_tasks: RelatedTask[];
	configuration: Array<{ label: string; value: string }>;
	consoleUrl?: string;
};

function buildSections(summary: VmSummary | null): { id: string; label: string; count?: number }[] {
	// Only list tabs the page actually renders. Volumes, networks, and related
	// tasks are shown on Summary; the full configuration grid lives in the
	// support rail. Declaring extra ids here produced blank panels on click.
	return [
		{ id: 'summary', label: 'Summary' },
		{ id: 'console', label: 'Console' },
		{ id: 'boot-log', label: 'Boot Log' },
		{ id: 'events', label: 'Events' },
		{ id: 'metrics', label: 'Metrics' },
		{ id: 'snapshots', label: 'Snapshots', count: summary?.snapshot_count ?? 0 }
	];
}

function buildConfiguration(summary: VmSummary): Array<{ label: string; value: string }> {
	return [
		{ label: 'VM ID', value: summary.vm_id },
		{ label: 'Name', value: summary.name },
		{ label: 'Node ID', value: summary.node_id },
		{ label: 'Power State', value: summary.power_state },
		{ label: 'Health', value: summary.health },
		{ label: 'CPU', value: summary.cpu },
		{ label: 'Memory', value: summary.memory }
	];
}

function buildDetailModel(summary: VmSummary | null, currentTab: string, consoleUrl?: string): VmDetailModel {
	if (!summary) {
		return {
			state: 'empty',
			currentTab,
			summary: {
				vm_id: '',
				name: '',
				node_id: '',
				power_state: '',
				health: '',
				cpu: '',
				memory: '',
				attached_volumes: [],
				attached_nics: []
			},
			sections: buildSections(null),
			recent_tasks: [],
			configuration: []
		};
	}

	return {
		state: 'ready',
		currentTab,
		summary: {
			vm_id: summary.vm_id,
			name: summary.name,
			node_id: summary.node_id,
			power_state: summary.power_state,
			health: summary.health,
			cpu: summary.cpu,
			memory: summary.memory,
			attached_volumes: summary.attached_volumes ?? [],
			attached_nics: summary.attached_nics ?? [],
			snapshot_count: summary.snapshot_count ?? 0
		},
		sections: buildSections(summary),
		recent_tasks: summary.recent_tasks ?? [],
		configuration: buildConfiguration(summary),
		consoleUrl
	};
}

export const load: PageLoad = async ({ params, url }) => {
	const token = getStoredToken() ?? undefined;
	const currentTab = resolveTab(url.searchParams.get('tab'));
	try {
		const res = await cachedFetch(
			`vms:detail:${params.id}`,
			() => getVm({ vm_id: params.id }, token),
			DETAIL_TTL
		);
		let consoleUrl: string | undefined;
		if (currentTab === 'console') {
			try {
				const consoleRes = await getVmConsoleUrl(params.id, token);
				consoleUrl = consoleRes.url;
			} catch (e) {
				console.warn('Failed to fetch console URL:', e);
			}
		}
		const detail = buildDetailModel(res.summary ?? null, currentTab, consoleUrl);
		return { detail, requestedVmId: params.id };
	} catch (err) {
		const message = err instanceof Error ? err.message : 'Failed to load VM details';
		console.error('[vm-detail-loader]', message, err);
		const detail: VmDetailModel = {
			state: 'error',
			currentTab,
			errorMessage: message,
			summary: {
				vm_id: params.id,
				name: '',
				node_id: '',
				power_state: '',
				health: '',
				cpu: '',
				memory: '',
				attached_volumes: [],
				attached_nics: []
			},
			sections: buildSections(null),
			recent_tasks: [],
			configuration: []
		};
		return { detail, requestedVmId: params.id };
	}
};
