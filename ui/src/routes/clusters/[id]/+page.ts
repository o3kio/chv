import type { PageLoad } from './$types';
import { getStoredToken } from '#lib/api/client.ts';
import { listClusters } from '#lib/bff/clusters.ts';

type ClusterListItem = {
	cluster_id: string;
	name: string;
	datacenter: string;
	node_count: number;
	state: string;
	maintenance: boolean;
	version: string;
	version_skew: boolean;
	cpu_percent: number;
	memory_percent: number;
	storage_percent: number;
	active_tasks: number;
	alerts: number;
	top_issue?: string;
};

export type ClusterDetailModel = {
	state: 'ready' | 'not_found' | 'error';
	summary: {
		clusterId: string;
		name: string;
		datacenter: string;
		nodeCount: number;
		state: string;
		maintenance: boolean;
		version: string;
		versionSkew: boolean;
		cpuPercent: number;
		memoryPercent: number;
		storagePercent: number;
		activeTasks: number;
		alerts: number;
		topIssue?: string;
	};
};

export const load: PageLoad = async ({ params }) => {
	const token = getStoredToken() ?? undefined;

	try {
		const response = await listClusters(token);
		const item = (response.items as ClusterListItem[]).find(
			(cluster) => cluster.cluster_id === params.id
		);

		if (!item) {
			return {
				detail: {
					state: 'not_found',
					summary: {
						clusterId: params.id,
						name: params.id,
						datacenter: '',
						nodeCount: 0,
						state: 'unknown',
						maintenance: false,
						version: 'unknown',
						versionSkew: false,
						cpuPercent: 0,
						memoryPercent: 0,
						storagePercent: 0,
						activeTasks: 0,
						alerts: 0
					}
				} satisfies ClusterDetailModel
			};
		}

		return {
			detail: {
				state: 'ready',
				summary: {
					clusterId: item.cluster_id,
					name: item.name,
					datacenter: item.datacenter,
					nodeCount: item.node_count,
					state: item.state,
					maintenance: item.maintenance,
					version: item.version,
					versionSkew: item.version_skew,
					cpuPercent: item.cpu_percent,
					memoryPercent: item.memory_percent,
					storagePercent: item.storage_percent,
					activeTasks: item.active_tasks,
					alerts: item.alerts,
					topIssue: item.top_issue
				}
			} satisfies ClusterDetailModel
		};
	} catch {
		return {
			detail: {
				state: 'error',
				summary: {
					clusterId: params.id,
					name: params.id,
					datacenter: '',
					nodeCount: 0,
					state: 'unknown',
					maintenance: false,
					version: 'unknown',
					versionSkew: false,
					cpuPercent: 0,
					memoryPercent: 0,
					storagePercent: 0,
					activeTasks: 0,
					alerts: 0
				}
			} satisfies ClusterDetailModel
		};
	}
};
