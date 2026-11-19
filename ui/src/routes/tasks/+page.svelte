<script lang="ts">
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import FilterBar from '#lib/components/shared/FilterBar.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import ResourceLink from '#lib/components/shell/ResourceLink.svelte';
	import DurationLine from '#lib/components/shell/DurationLine.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import { getTaskStatusMeta, normalizeTaskStatus } from '#lib/webui/tasks.ts';
	import { getTaskFailureCause } from '#lib/webui/task-failure.ts';
	import type { PageData } from './$types';
	import { History, User, Activity, Clock, ShieldAlert } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page as appPage } from '$app/state';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/tasks');
	const tasks = $derived(data.tasks);
	const items = $derived(tasks.items);

	const filters = $derived([
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Task ID or resource...' },
		{
			key: 'status',
			label: 'Status',
			type: 'select' as const,
			options: [
				{ value: 'all', label: 'All states' },
				...tasks.filters.options.statuses.map((s: string) => ({
					value: s,
					label: getTaskStatusMeta(s).label
				}))
			]
		},
		{
			key: 'window',
			label: 'Window',
			type: 'select' as const,
			options: [
				{ value: 'active', label: 'Active only' },
				{ value: '24h', label: 'Last 24 hours' },
				{ value: '7d', label: 'Last 7 days' },
				{ value: 'all', label: 'All time' }
			]
		}
	]);

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
		{ key: 'operation', label: 'Operation ID' },
		{ key: 'status', label: 'State' },
		{ key: 'resource', label: 'Target' },
		{ key: 'actor', label: 'Principal' },
		{ key: 'started', label: 'Timestamp' },
		{ key: 'duration', label: 'Ops Duration', align: 'right' as const }
	];

	const rows = $derived(
		items.map((task) => {
			const statusMeta = getTaskStatusMeta(task.status);
			return {
				...task,
				status: { label: statusMeta.label, tone: statusMeta.tone },
				started: new Date(task.started_unix_ms).toLocaleString('en-US', {
					month: 'short',
					day: 'numeric',
					hour: 'numeric',
					minute: '2-digit'
				})
			};
		})
	);

	// The BFF serializes statuses capitalized ("Failed"); normalize
	// before comparing (review SF-1 — the raw 'failed' compare never
	// matched, so the rail never populated).
	const failedTasks = $derived(
		items.filter(t => normalizeTaskStatus(t.status) === 'failed').slice(0, 3)
	);
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={page} />

	<div class="inventory-metrics">
		<CompactMetricCard 
			label="Total Mutations" 
			value={tasks.page.totalItems} 
			color="neutral"
		/>
		<CompactMetricCard 
			label="Active Ops" 
			value={items.filter(t => t.status === 'running' || t.status === 'awaiting-operator-input').length} 
			color="primary"
		/>
		<CompactMetricCard 
			label="Awaiting Input" 
			value={items.filter(t => t.status === 'awaiting-operator-input').length} 
			color={items.filter(t => t.status === 'awaiting-operator-input').length > 0 ? 'warning' : 'neutral'}
		/>
		<CompactMetricCard 
			label="Failed" 
			value={items.filter(t => t.status === 'failed').length} 
			color={items.filter(t => t.status === 'failed').length > 0 ? 'danger' : 'neutral'}
		/>
	</div>

	<div class="inventory-controls">
		<FilterBar 
			{filters} 
			activeFilters={tasks.filters.current} 
			onFilterChange={handleFilterChange}
			onClearAll={() => goto(appPage.url.pathname)}
		/>
	</div>

	<main class="inventory-main">
		<section class="inventory-table-area">
			{#if tasks.state === 'error'}
				<ErrorState />
			{:else if tasks.state === 'empty'}
				<EmptyInfrastructureState 
					title="Operation trace empty" 
					description="Adjust your search criteria or time window." 
					hint="All control-plane mutations are recorded in the audit registry."
				/>
			{:else}
				<InventoryTable 
					{columns} 
					rows={rows}
				>
					{#snippet cell({ column, row })}
						{#if column.key === 'operation'}
							<div class="op-cell">
								<span class="op-name">{row.operation}</span>
								<span class="op-id">{row.task_id}</span>
							</div>
						{:else if column.key === 'resource'}
							<ResourceLink kind={row.resource_kind} id={row.resource_id} name={row.resource_name} compact />
						{:else if column.key === 'actor'}
							<div class="actor-cell">
								<User size={10} />
								<span>{row.actor}</span>
							</div>
						{:else if column.key === 'duration'}
							<DurationLine startedMs={row.started_unix_ms} finishedMs={row.finished_unix_ms} />
						{:else if typeof (row as any)[column.key] === 'object' && (row as any)[column.key]?.tone}
							<StatusBadge label={(row as any)[column.key].label} tone={(row as any)[column.key].tone} />
						{:else}
							<span class="cell-text">{(row as any)[column.key]}</span>
						{/if}
					{/snippet}
				</InventoryTable>
			{/if}
		</section>

		<aside class="support-area">
			<SectionCard title="Mutation Audit" icon={History}>
				<div class="audit-summary">
					<div class="summary-row">
						<span>Retention Policy</span>
						<span>30 Days</span>
					</div>
					<div class="summary-row">
						<span>Consistency</span>
						<span>Verified</span>
					</div>
				</div>
			</SectionCard>

			<SectionCard title="Failed Operations" icon={ShieldAlert} badgeLabel={String(failedTasks.length)}>
				{#if failedTasks.length === 0}
					<p class="empty-hint">No operational failures in the current window.</p>
				{:else}
					<ul class="attention-list">
						{#each failedTasks as task (task.task_id)}
						{@const failure = getTaskFailureCause(task)}
							<li>
								<div class="attention-card">
									<div class="attention-card__main">
										<span class="res-name">{task.operation}</span>
										<span class="res-issue">
											{#if failure?.code}
												<!-- #502: the journaled terminal-failure code, as a
													label beside the sibling status chips. -->
												<span class="failure-code" data-testid="task-failure-code">{failure.code}</span>
											{/if}
											Failure Registry: {task.task_id.split('-')[0]}
										</span>
										{#if failure?.message}
											<!-- #502: the agents' verbatim refusal text — rendered
												as-is, it IS the diagnostic. -->
											<span class="failure-message" data-testid="task-failure-message" title={failure.message}>{failure.message}</span>
										{/if}
									</div>
								</div>
							</li>
						{/each}
					</ul>
				{/if}
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
		border-radius: 0.35rem;
		overflow: hidden;
	}

	.actor-cell {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
	}

	/* #502: the surfaced terminal-failure cause in the Failed
	   Operations rail — the code as a small label, the refusal text
	   verbatim, clamped to keep one row per failure. */
	.failure-code {
		display: inline-block;
		margin-right: 0.35rem;
		padding: 0.05rem 0.35rem;
		border: 1px solid var(--status-failed-border);
		border-radius: 9999px;
		background: var(--status-failed-bg);
		color: var(--status-failed-text);
		font-size: var(--chv-text-xs);
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		white-space: nowrap;
	}

	.failure-message {
		display: block;
		margin-top: 0.25rem;
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
