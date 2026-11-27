<script lang="ts">
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { ShellTone } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import { Clock, ArrowUpFromLine, Activity } from 'lucide-svelte';
	import { buildMaintenanceView } from '#lib/webui/maintenance.ts';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/maintenance');
	const maintenance = $derived(buildMaintenanceView(data.maintenance));

	const evacuatingCount = $derived(
		maintenance.nodes.filter((node) => node.state === 'draining').length
	);
	const inMaintenanceCount = $derived(
		maintenance.nodes.filter((node) => node.state === 'in_maintenance').length
	);

	function statusTone(status: string): ShellTone {
		const s = status.toLowerCase();
		if (['active', 'scheduled', 'draining', 'in_maintenance'].includes(s)) return 'warning';
		if (['completed', 'ready'].includes(s)) return 'healthy';
		return 'unknown';
	}
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={page} />

	{#if data.error}
		<ErrorState />
	{:else if maintenance.empty}
		<EmptyInfrastructureState
			title="Lifecycle registry empty"
			description="No active maintenance windows or node evacuations detected."
			hint="Drain compute nodes before performing hardware or kernel upgrades."
		/>
	{:else}
		<div class="inventory-metrics">
			<CompactMetricCard
				label="Evacuating Nodes"
				value={evacuatingCount}
				color="warning"
			/>
			<CompactMetricCard
				label="In Maintenance"
				value={inMaintenanceCount}
				color="neutral"
			/>
			<CompactMetricCard
				label="Operator Actions"
				value={maintenance.pendingActions}
				color={maintenance.pendingActions > 0 ? 'danger' : 'neutral'}
			/>
		</div>

		<main class="inventory-main">
			<div class="main-content-flow">
				<SectionCard title="Active Operational Windows" icon={Clock} badgeLabel={String(maintenance.windows.length)}>
					{#if maintenance.windows.length === 0}
						<p class="empty-hint">No scheduled lifecycle windows detected.</p>
					{:else}
						<div class="window-registry">
							{#each maintenance.windows as window (window.window_id)}
								<div class="window-entry">
									<div class="window-main">
										<span class="window-title">{window.name}</span>
										<div class="window-meta">
											<span>Starts: {window.startsAt}</span>
											<span>Ends: {window.endsAt}</span>
										</div>
									</div>
									<StatusBadge label={window.status} tone={statusTone(window.status)} />
								</div>
							{/each}
						</div>
					{/if}
				</SectionCard>

				<SectionCard title="Node Maintenance State" icon={ArrowUpFromLine}>
					{#if maintenance.nodes.length === 0}
						<p class="empty-hint">No node is reported in a maintenance or draining state.</p>
					{:else}
						<div class="node-op-registry">
							{#each maintenance.nodes as node (node.node_id)}
								<div class="node-op-entry">
									<div class="node-op-header">
										<div class="node-info">
											<span class="node-name">{node.name}</span>
											<span class="node-status">{node.node_id}</span>
										</div>
										<StatusBadge label={node.state} tone={statusTone(node.state)} />
									</div>
								</div>
							{/each}
						</div>
					{/if}
				</SectionCard>
			</div>

			<aside class="support-area">
				<SectionCard title="Reported Lifecycle State" icon={Activity}>
					<div class="audit-summary">
						<div class="summary-row">
							<span>Windows in view</span>
							<span>{maintenance.windows.length}</span>
						</div>
						<div class="summary-row">
							<span>Nodes in view</span>
							<span>{maintenance.nodes.length}</span>
						</div>
						<div class="summary-row">
							<span>Pending operator actions</span>
							<span>{maintenance.pendingActions}</span>
						</div>
					</div>
				</SectionCard>
			</aside>
		</main>
	{/if}
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

	.inventory-main {
		display: grid;
		grid-template-columns: 1fr 300px;
		gap: 1rem;
		align-items: start;
	}

	.main-content-flow {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.window-registry {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.window-entry {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 1rem;
		padding: 0.5rem 0.75rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.window-main {
		display: flex;
		flex-direction: column;
		gap: 1px;
	}

	.window-title {
		font-size: 11px;
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.window-meta {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
		font-size: 10px;
		color: var(--chv-color-neutral-500);
	}

	.node-op-registry {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.node-op-entry {
		padding: 0.5rem 0.75rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.node-op-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 1rem;
	}

	.node-info {
		display: flex;
		flex-direction: column;
		gap: 1px;
	}

	.node-name {
		font-size: 11px;
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.node-status {
		font-size: 10px;
		color: var(--chv-color-neutral-500);
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
		font-size: 11px;
		color: var(--chv-color-neutral-400);
		padding: 1rem;
		text-align: center;
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}
</style>
