<script lang="ts">
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import FilterBar from '#lib/components/shared/FilterBar.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import ResourceLink from '#lib/components/shell/ResourceLink.svelte';
	import SeverityShield from '#lib/components/shell/SeverityShield.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import { Bell, AlertTriangle, ShieldAlert, Activity, Filter } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page as appPage } from '$app/state';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/events');
	const model = $derived(data.events);
	const items = $derived(model.items);

	const filters = [
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Summary or resource...' },
		{
			key: 'severity',
			label: 'Severity',
			type: 'select' as const,
			options: [
				{ value: 'all', label: 'All severities' },
				{ value: 'critical', label: 'Critical' },
				{ value: 'warning', label: 'Warning' },
				{ value: 'info', label: 'Info' }
			]
		},
		{
			key: 'type',
			label: 'Type',
			type: 'select' as const,
			options: [
				{ value: 'all', label: 'All types' },
				{ value: 'vm_lifecycle', label: 'VM Lifecycle' },
				{ value: 'storage', label: 'Storage' },
				{ value: 'network', label: 'Network' },
				{ value: 'security', label: 'Security' },
				{ value: 'system', label: 'System' },
				{ value: 'migration', label: 'Migration' }
			]
		},
		{
			key: 'resource_kind',
			label: 'Resource',
			type: 'select' as const,
			options: [
				{ value: 'all', label: 'All resources' },
				{ value: 'vm', label: 'VM' },
				{ value: 'node', label: 'Node' },
				{ value: 'volume', label: 'Volume' },
				{ value: 'network', label: 'Network' },
				{ value: 'image', label: 'Image' }
			]
		},
		{
			key: 'state',
			label: 'State',
			type: 'select' as const,
			options: [
				{ value: 'all', label: 'All states' },
				{ value: 'open', label: 'Open' },
				{ value: 'acknowledged', label: 'Acknowledged' },
				{ value: 'resolved', label: 'Resolved' }
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
		{ key: 'severity', label: 'Priority', align: 'center' as const },
		{ key: 'summary', label: 'Anomaly Summary' },
		{ key: 'resource', label: 'Detected Resource' },
		{ key: 'type', label: 'Domain' },
		{ key: 'state', label: 'Audit State' },
		{ key: 'occurred', label: 'Sequence Time', align: 'right' as const }
	];

	const rows = $derived(
		items.map((item) => ({
			...item,
			occurred: new Date(item.occurred_at).toLocaleString('en-US', {
				month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit'
			})
		}))
	);

	const criticalEvents = $derived(items.filter(e => e.severity === 'critical' && e.state !== 'resolved').slice(0, 3));
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={page} />

	<div class="inventory-metrics">
		<CompactMetricCard 
			label="Incident Count" 
			value={items.length} 
			color="neutral"
		/>
		<CompactMetricCard 
			label="Unresolved Faults" 
			value={items.filter(e => e.severity === 'critical' && e.state !== 'resolved').length} 
			color={items.filter(e => e.severity === 'critical' && e.state !== 'resolved').length > 0 ? 'danger' : 'neutral'}
		/>
		<CompactMetricCard 
			label="Active Warnings" 
			value={items.filter(e => e.severity === 'warning' && e.state !== 'resolved').length} 
			color="warning"
		/>
		<CompactMetricCard 
			label="Audit Pipeline" 
			value="NOMINAL" 
			color="primary"
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
					title="Incidence registry clear" 
					description="Adjust your search criteria or domain filters." 
					hint="System incidents are recorded in the central diagnostic registry."
				/>
			{:else}
				<InventoryTable {columns} rows={rows}>
					{#snippet cell({ column, row })}
						{#if column.key === 'severity'}
							<SeverityShield severity={row.severity} />
						{:else if column.key === 'resource'}
							<ResourceLink kind={row.resource_kind} id={row.resource_id} name={row.resource_name} compact />
						{:else if column.key === 'state'}
							<StatusBadge 
								label={row.state} 
								tone={row.state === 'resolved' ? 'healthy' : row.state === 'open' ? 'failed' : 'warning'} 
							/>
						{:else if column.key === 'summary'}
							<span class="summary-text">{row.summary}</span>
						{:else}
							<span class="cell-text">{(row as Record<string, unknown>)[column.key]}</span>
						{/if}
					{/snippet}
				</InventoryTable>
			{/if}
		</section>

		<aside class="support-area">
			<SectionCard title="Anomaly Priority" icon={ShieldAlert} badgeLabel={String(criticalEvents.length)}>
				{#if criticalEvents.length === 0}
					<p class="empty-hint">All systems reporting nominal telemetry.</p>
				{:else}
					<ul class="attention-list">
						{#each criticalEvents as event}
							<li>
								<div class="attention-card">
									<div class="attention-card__main">
										<span class="res-name">{event.summary}</span>
										<span class="res-issue">{event.resource_name} (Fault)</span>
									</div>
								</div>
							</li>
						{/each}
					</ul>
				{/if}
			</SectionCard>

			<SectionCard title="Entropy Tracking" icon={Activity}>
				<div class="audit-summary">
					<div class="summary-row">
						<span>Incidents (24h)</span>
						<span>{items.filter(e => new Date(e.occurred_at).getTime() > Date.now() - 86400000).length}</span>
					</div>
					<div class="summary-row">
						<span>Resolution Ratio</span>
						<span>{items.length ? Math.round((items.filter(e => e.state === 'resolved').length / items.length) * 100) : 100}%</span>
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

	.summary-text {
		font-weight: 800;
		color: var(--chv-color-neutral-900);
    font-size: 11px;
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
    border-left-color: var(--color-danger);
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
		color: var(--color-danger);
		font-weight: 700;
		text-transform: uppercase;
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}
</style>
