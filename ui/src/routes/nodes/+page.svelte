<script lang="ts">
import type { PageData } from './$types';
	import InventoryListPage from '#lib/components/shell/InventoryListPage.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import { isNodeReady, nodeStateTone } from '#lib/webui/node-state.ts';
	import { AlertCircle } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';

	let { data }: { data: PageData } = $props();

	const model = $derived(data.nodes);
	const items = $derived(model.items);

	const pageDef = getPageDefinition('/nodes');

	const filters = [
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Filter by node name or cluster...' },
		{
			key: 'state',
			label: 'State',
			type: 'select' as const,
			options: [
				{ value: 'HostReady', label: 'Host Ready' },
				{ value: 'StorageReady', label: 'Storage Ready' },
				{ value: 'NetworkReady', label: 'Network Ready' },
				{ value: 'TenantReady', label: 'Tenant Ready' },
				{ value: 'Degraded', label: 'Degraded' },
				{ value: 'Draining', label: 'Draining' },
				{ value: 'Maintenance', label: 'Maintenance' },
				{ value: 'Failed', label: 'Failed' },
				{ value: 'Unreachable', label: 'Unreachable' },
				{ value: 'Discovered', label: 'Discovered' },
				{ value: 'Bootstrapping', label: 'Bootstrapping' },
				{ value: 'Unknown', label: 'Unknown' }
			]
		},
		{ key: 'maintenance', label: 'Maintenance', type: 'boolean' as const }
	];

	function handleFilterChange(key: string, value: unknown) {
		const newParams = new URLSearchParams(page.url.searchParams.toString());
		if (value === '' || value === 'all' || value === false) {
			newParams.delete(key);
		} else {
			newParams.set(key, String(value));
		}
		goto(`?${newParams.toString()}`, { reset: false });
	}

	function handleClearFilters() {
		goto(page.url.pathname);
	}

	const columns = [
		{ key: 'name', label: 'Compute Node' },
		{ key: 'cluster', label: 'Cluster Assignment' },
		{ key: 'state', label: 'Status' },
		{ key: 'cpu', label: 'CPU Index', align: 'right' as const },
		{ key: 'memory', label: 'Memory Index', align: 'right' as const },
		{ key: 'storage', label: 'Storage Index', align: 'right' as const },
		{ key: 'hypervisor_capabilities', label: 'Host Features' },
		{ key: 'version', label: 'Platform Rev' }
	];

	const tableRows = $derived(items.map(item => ({
		...item,
		state: { label: item.state, tone: nodeStateTone(item.state, item.health) },
		hypervisor_capabilities: item.hypervisor_capabilities?.includes('kvm') ? 'KVM' : '—'
	})));

	const attentionNodes = $derived(items.filter(n => n.health !== 'healthy' || n.alerts > 0).slice(0, 3));

	const metrics = $derived([
		{ label: 'Compute Capacity', value: items.length, color: 'neutral' as const },
		{ label: 'Operational', value: items.filter(n => isNodeReady(n.state)).length, color: 'primary' as const },
		{ label: 'Posture Warning', value: items.filter(n => n.health !== 'healthy').length, color: items.filter(n => n.health !== 'healthy').length > 0 ? 'warning' as const : 'neutral' as const }
	]);
</script>

<InventoryListPage
	page={pageDef}
	{filters}
	activeFilters={model.filters.current}
	{metrics}
	onFilterChange={handleFilterChange}
	onClearFilters={handleClearFilters}
	state={model.state}
	emptyTitle="No nodes detected"
	emptyDescription="Adjust your search criteria or enroll a new compute host."
	emptyHint="New hosts must be enrolled via the control-plane CLI."
	{columns}
	rows={tableRows}
	rowHref={(row) => `/nodes/${row.node_id}`}
>
	{#snippet sidebar()}
		<SectionCard title="Host Posture" icon={AlertCircle} badgeLabel={String(attentionNodes.length)}>
			{#if attentionNodes.length === 0}
				<p class="empty-hint">No host reports degradation or active alerts.</p>
			{:else}
				<ul class="attention-list">
					{#each attentionNodes as node}
						<li>
							<a href={`/nodes/${node.node_id}`} class="attention-card">
								<div class="attention-card__main">
									<span class="res-name">{node.name}</span>
									<span class="res-issue">{node.alerts} alerts / {node.health}</span>
								</div>
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</SectionCard>
	{/snippet}
</InventoryListPage>

<style>
</style>
