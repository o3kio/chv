<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import FilterBar from '#lib/components/shared/FilterBar.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import { Blocks, AlertTriangle, Activity, Plus, LayoutGrid } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page as appPage } from '$app/state';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/clusters');
	const model = $derived(data.clusters);
	const items = $derived(model.items);

	const filters = [
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Cluster or DC name...' },
		{
			key: 'state',
			label: 'Readiness',
			type: 'select' as const,
			options: [
				{ value: 'all', label: 'All states' },
				{ value: 'healthy', label: 'Healthy' },
				{ value: 'degraded', label: 'Degraded' },
				{ value: 'maintenance', label: 'Maintenance' }
			]
		}
	];

	function handleFilterChange(key: string, value: any) {
		const newParams = new URLSearchParams(appPage.url.searchParams.toString());
		if (value === '' || value === 'all') {
			newParams.delete(key);
		} else {
			newParams.set(key, String(value));
		}
		goto(`?${newParams.toString()}`, { reset: false });
	}

	const columns = [
		{ key: 'name', label: 'Compute Fabric' },
		{ key: 'datacenter', label: 'Zone' },
		{ key: 'node_count', label: 'Enrolled Nodes', align: 'center' as const },
		{ key: 'state', label: 'Readiness' },
		{ key: 'capacity', label: 'Peak Pressure' },
		{ key: 'active_tasks', label: 'Ops', align: 'right' as const }
	];

	const rows = $derived(
		items.map((item) => {
			const peak = Math.max(item.cpu_percent, item.memory_percent, item.storage_percent);
			return {
				...item,
				capacity: { 
					label: `${peak}% PRESSURE`, 
					tone: peak >= 85 ? 'failed' : peak >= 70 ? 'degraded' : peak >= 50 ? 'warning' : 'healthy' 
				},
				state: {
					label: item.state,
					tone: item.state === 'healthy' ? 'healthy' : item.state === 'degraded' ? 'degraded' : 'failed'
				}
			};
		})
	);

	const attentionClusters = $derived(items.filter(c => c.state !== 'healthy' || c.alerts > 0).slice(0, 3));
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={page}>
		{#snippet actions()}
			<Button variant="primary">
				<Plus size={14} />
				Enroll Fabric
			</Button>
		{/snippet}
	</PageHeaderWithAction>

	<div class="inventory-metrics">
		<CompactMetricCard 
			label="Total Fabrics" 
			value={items.length} 
			color="neutral"
		/>
		<CompactMetricCard 
			label="Nominal Load" 
			value={items.filter(c => c.state === 'healthy').length} 
			color="primary"
		/>
		<CompactMetricCard 
			label="System Alerts" 
			value={items.reduce((sum, c) => sum + (c.alerts || 0), 0)} 
			color={items.reduce((sum, c) => sum + (c.alerts || 0), 0) > 0 ? 'warning' : 'neutral'}
		/>
	</div>

	<div class="inventory-controls">
		<FilterBar 
			{filters} 
			activeFilters={model.filters.current} 
			onFilterChange={handleFilterChange}
			onClearAll={() => goto(appPage.url.pathname)}
		/>
	</div>

	<main class="inventory-main">
		<section class="inventory-table-area">
			{#if model.state === 'error'}
				<ErrorState />
			{:else if model.state === 'empty'}
				<EmptyInfrastructureState 
					title="No fabrics cataloged" 
					description="Adjust your search criteria or enroll a new cluster fabric." 
					hint="Fabrics represent logical groupings of heterogeneous compute resources."
				/>
			{:else}
				<InventoryTable 
					{columns} 
					rows={rows}
					rowHref={(row) => `/clusters/${row.cluster_id}`}
				>
					{#snippet cell({ column, row })}
						{@const val = (row as any)[column.key]}
						{#if column.key === 'name'}
							<div class="fabric-identity">
								<span class="fabric-name">{row.name}</span>
								{#if (row as any).is_local}
									<span class="fabric-tag">EDGE</span>
								{/if}
							</div>
						{:else if typeof val === 'object' && val?.tone}
							<StatusBadge label={val.label} tone={val.tone} />
						{:else}
							<span class="cell-text">{val}</span>
						{/if}
					{/snippet}
				</InventoryTable>
			{/if}
		</section>

		<aside class="support-area">
			<SectionCard title="Priority Inspection" icon={AlertTriangle} badgeLabel={String(attentionClusters.length)}>
				{#if attentionClusters.length === 0}
					<p class="empty-hint">All cluster fabrics reporting nominal readiness.</p>
				{:else}
					<ul class="attention-list">
						{#each attentionClusters as cluster}
							<li>
								<div class="attention-card">
									<div class="attention-card__main">
										<span class="res-name">{cluster.name}</span>
										<span class="res-issue">{cluster.alerts} anomaly detections</span>
									</div>
								</div>
							</li>
						{/each}
					</ul>
				{/if}
			</SectionCard>

			<SectionCard title="Fabric Telemetry" icon={LayoutGrid}>
				<div class="audit-summary">
					<div class="summary-row">
						<span>Total Compute Blocks</span>
						<span>{items.reduce((sum, c) => sum + c.node_count, 0)}</span>
					</div>
					<div class="summary-row">
						<span>Load Balancing</span>
						<span>Automatic</span>
					</div>
				</div>
			</SectionCard>
		</aside>
	</main>
</div>

<style>
	.inventory-page {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.inventory-metrics {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: 0.75rem;
	}

	.inventory-controls {
		background: var(--bg-surface);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
		overflow: hidden;
	}

	.inventory-main {
		display: grid;
		grid-template-columns: 1fr 300px;
		gap: 1rem;
		align-items: start;
	}

	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.fabric-identity {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.fabric-name {
		font-weight: 800;
		color: var(--chv-color-neutral-900);
	}

	.fabric-tag {
		font-size: 8px;
		font-weight: 800;
		color: #ffffff;
		background: var(--color-primary);
		padding: 1px 4px;
		border-radius: 2px;
	}

	.cell-text {
		font-size: 11px;
		color: var(--chv-color-neutral-600);
	}

	.audit-summary {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.summary-row {
		display: flex;
		justify-content: space-between;
		font-size: 10px;
		color: var(--chv-color-neutral-600);
		padding: 0.35rem 0.5rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.summary-row span:last-child {
		font-weight: 800;
		color: var(--chv-color-neutral-900);
	}

	.empty-hint {
		font-size: 10px;
		font-weight: 700;
		color: var(--chv-color-neutral-400);
		padding: 1rem;
		text-align: center;
		text-transform: uppercase;
	}

	.attention-list {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		list-style: none;
		padding: 0;
		margin: 0;
	}

	.attention-card {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0.5rem 0.75rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
		color: var(--chv-color-neutral-800);
    border-left: 2px solid transparent;
	}

  .attention-card:has(.res-issue) {
    border-left-color: var(--color-warning);
  }

	.attention-card__main {
		display: flex;
		flex-direction: column;
		gap: 0.125rem;
	}

	.res-name {
		font-size: 11px;
		font-weight: 800;
    color: var(--chv-color-neutral-900);
	}

	.res-issue {
		font-size: 9px;
		color: var(--color-warning);
		font-weight: 700;
		text-transform: uppercase;
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}
</style>
