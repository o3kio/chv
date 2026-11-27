import type { PageLoad } from './$types';
import { getStoredToken } from '#lib/api/client.ts';
import { listNodes } from '#lib/bff/nodes.ts';
import { cachedFetch, LIST_TTL } from '#lib/stores/api-cache.svelte.ts';
import type { ListNodesRequest, NodeListItem } from '#lib/bff/types.ts';
import { matchesNodeStateFilter } from '#lib/webui/node-state.ts';

export type { NodeListItem };

type NodesListModel = {
	items: NodeListItem[];
	state: 'ready' | 'empty' | 'error';
	filters: { current: Record<string, string>; applied: Record<string, string> };
	page: { page: number; pageSize: number; totalItems: number };
};

function filterNodes(items: NodeListItem[], current: Record<string, string>): NodeListItem[] {
	let result = [...items];
	const query = (current.query ?? '').toLowerCase();
	if (query) {
		result = result.filter(
			(n) => n.name.toLowerCase().includes(query) || n.cluster.toLowerCase().includes(query)
		);
	}
	const state = current.state;
	if (state && state !== 'all') {
		result = result.filter((n) => matchesNodeStateFilter(n.state, state));
	}
	const maintenance = current.maintenance;
	if (maintenance && maintenance !== 'all') {
		result = result.filter((n) => (n.maintenance ? 'true' : 'false') === maintenance);
	}
	return result;
}

export const load: PageLoad = async ({ url }) => {
	const token = getStoredToken() ?? undefined;
	const page = Math.max(1, parseInt(url.searchParams.get('page') ?? '1', 10) || 1);
	const pageSize = 50;

	const current: Record<string, string> = {};
	const query = url.searchParams.get('query');
	const state = url.searchParams.get('state');
	const maintenance = url.searchParams.get('maintenance');

	if (query) current.query = query;
	if (state) current.state = state;
	if (maintenance) current.maintenance = maintenance;

	const req: ListNodesRequest = {
		page,
		page_size: pageSize,
		filters: current
	};

	try {
		const res = await cachedFetch(
			`nodes:list:${JSON.stringify(req)}`,
			() => listNodes(req, token),
			LIST_TTL
		);
		const fetchedItems = res.items;

		const filtered = filterNodes(fetchedItems, current);

		const model: NodesListModel = {
			items: filtered,
			state: filtered.length === 0 ? 'empty' : 'ready',
			filters: { current, applied: res.filters?.applied ?? current },
			page: {
				page,
				pageSize,
				totalItems: res.page.total_items
			}
		};

		return { nodes: model };
	} catch (err) {
		// eslint-disable-next-line no-console
		console.error('BFF listNodes error:', err);
		const model: NodesListModel = {
			items: [],
			state: 'error',
			filters: { current, applied: {} },
			page: { page, pageSize, totalItems: 0 }
		};
		return { nodes: model };
	}
};
