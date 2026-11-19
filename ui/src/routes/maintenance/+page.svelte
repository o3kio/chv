<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import ProgressBar from '#lib/components/shell/ProgressBar.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import { Wrench, ArrowUpFromLine, RefreshCcw, Activity, ShieldCheck, AlertCircle, Clock } from 'lucide-svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';

	let { data }: { data: PageData } = $props();

	const page = getPageDefinition('/maintenance');
	const maintenance = $derived(data.maintenance);

	const activeWindows = $derived(maintenance.windows);
	const drainingNodes = $derived(maintenance.nodes);

	const isEmpty = $derived(
		activeWindows.length === 0 &&
		drainingNodes.length === 0 &&
		!maintenance.pending_actions &&
		!maintenance.upgrade_available
	);

	const upgradePostureProps = $derived([
		{ label: 'Current Release', value: maintenance.current_version || 'Unavailable' },
		{ label: 'Platform Readiness', value: maintenance.upgrade_available ? 'Upgrade Available' : 'Current', tone: (maintenance.upgrade_available ? 'warning' : 'healthy') as any },
		{ label: 'Node Reboot Requirement', value: (maintenance.reboot_required_nodes?.length ?? 0) > 0 ? `${maintenance.reboot_required_nodes!.length} Pending` : 'Nominal', tone: (maintenance.reboot_required_nodes?.length ? 'warning' : 'healthy') as any },
		{ label: 'Orchestrator State', value: maintenance.orchestrator_health || 'Nominal', tone: (maintenance.orchestrator_health === 'Nominal' ? 'healthy' : 'warning') as any }
	]);
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={page}>
		{#snippet actions()}
			<Button variant="primary">
				<Wrench size={14} />
				Schedule Sequence
			</Button>
		{/snippet}
	</PageHeaderWithAction>

	{#if data.error}
		<ErrorState />
	{:else if isEmpty}
		<EmptyInfrastructureState
			title="Lifecycle registry empty"
			description="No active maintenance windows or node evacuations detected."
			hint="Drain compute nodes before performing hardware or kernel upgrades."
		/>
	{:else}
		<div class="inventory-metrics">
			<CompactMetricCard 
				label="Evacuating Nodes" 
				value={drainingNodes.filter(n => n.status === 'draining').length} 
				color="warning"
			/>
			<CompactMetricCard 
				label="In Maintenance" 
				value={drainingNodes.filter(n => n.status === 'in_maintenance').length} 
				color="neutral"
			/>
			<CompactMetricCard 
				label="Operator Actions" 
				value={maintenance.pending_actions || 0} 
				color={maintenance.pending_actions > 0 ? 'danger' : 'neutral'}
			/>
			<CompactMetricCard 
				label="Upgrade State" 
				value={maintenance.upgrade_available ? 'Pending' : 'Stable'} 
				color={maintenance.upgrade_available ? 'warning' : 'primary'}
			/>
		</div>

		<main class="inventory-main">
			<div class="main-content-flow">
				<SectionCard title="Active Operational Windows" icon={Clock} badgeLabel={String(activeWindows.length)}>
					{#if activeWindows.length === 0}
						<p class="empty-hint">No scheduled lifecycle windows detected.</p>
					{:else}
						<div class="window-registry">
							{#each activeWindows as window}
								<div class="window-entry">
									<div class="window-main">
										<span class="window-title">{window.title}</span>
										<div class="window-meta">
											<span>Target EOF: {new Date(window.expected_end_at).toLocaleDateString()}</span>
										</div>
									</div>
									<StatusBadge label={window.status} tone="warning" />
								</div>
							{/each}
						</div>
					{/if}
				</SectionCard>

				<SectionCard title="Evacuation Progress" icon={ArrowUpFromLine}>
					{#if drainingNodes.length === 0}
						<p class="empty-hint">All compute nodes currently in nominal operational state.</p>
					{:else}
						<div class="node-op-registry">
							{#each drainingNodes as node}
								<div class="node-op-entry">
									<div class="node-op-header">
										<div class="node-info">
											<span class="node-name">{node.name}</span>
											<span class="node-status">{node.status}</span>
										</div>
										<span class="progress-pct">{node.progress}%</span>
									</div>
									<ProgressBar progress={node.progress || 0} tone={node.status === 'draining' ? 'warning' : 'healthy'} size="sm" />
								</div>
							{/each}
						</div>
					{/if}
				</SectionCard>

				<SectionCard title="Platform Revision Posture" icon={RefreshCcw}>
					<PropertyGrid properties={upgradePostureProps} columns={2} />
					<div class="upgrade-dispatch">
						{#if maintenance.upgrade_available}
							<div class="upgrade-alert">
								<span class="spin-slow"><RefreshCcw size={14} /></span>
								<div class="upgrade-text">
									<strong>v2.5.0-LTS STAGED</strong>
									<span>Includes critical NVMe driver stability patches.</span>
								</div>
								<Button variant="primary">Initiate Upgrade</Button>
							</div>
						{:else}
							<div class="posture-nominal">
								<ShieldCheck size={14} />
								<span>Platform core is synchronized with latest stable artifacts.</span>
							</div>
						{/if}
					</div>
				</SectionCard>
			</div>

			<aside class="support-area">
				<SectionCard title="Principal Interventions" icon={AlertCircle} badgeLabel={String((maintenance.pending_operator_actions || []).length)}>
					{#if (maintenance.pending_operator_actions || []).length === 0}
						<p class="empty-hint">No manual interventions required.</p>
					{:else}
						<ul class="action-list">
							{#each maintenance.pending_operator_actions as action}
								<li>
									<div class="action-item">
										<span class="action-desc">{action.summary}</span>
										<Button variant="secondary">Acknowledge</Button>
									</div>
								</li>
							{/each}
						</ul>
					{/if}
				</SectionCard>

				<SectionCard title="Lifecycle Audit" icon={Activity}>
					<div class="audit-summary">
						<div class="summary-row">
							<span>Last Upgrade</span>
							<span>12d ago</span>
						</div>
						<div class="summary-row">
							<span>Success Rate</span>
							<span>100%</span>
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
		font-weight: 800;
    color: var(--chv-color-neutral-900);
	}

	.window-meta {
		font-size: 9px;
		color: var(--chv-color-neutral-400);
    font-weight: 700;
	}

	.node-op-registry {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.node-op-entry {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.node-op-header {
		display: flex;
		justify-content: space-between;
		align-items: flex-end;
	}

	.node-info {
		display: flex;
		flex-direction: column;
    gap: 1px;
	}

	.node-name {
		font-size: 11px;
		font-weight: 800;
    color: var(--chv-color-neutral-900);
	}

	.node-status {
		font-size: 9px;
		text-transform: uppercase;
		color: var(--chv-color-neutral-400);
		font-weight: 700;
	}

	.progress-pct {
		font-family: var(--chv-font-mono);
		font-size: 10px;
		font-weight: 800;
    color: var(--chv-color-neutral-600);
	}

	.upgrade-dispatch {
		margin-top: 1rem;
		padding-top: 1rem;
		border-top: 1px solid var(--border-subtle);
	}

	.upgrade-alert {
		display: flex;
		align-items: center;
		gap: 1rem;
		padding: 0.75rem;
		background: rgba(var(--color-warning-rgb), 0.1);
		border: 1px solid var(--color-warning);
		border-radius: var(--radius-xs);
	}

	.upgrade-text {
		flex: 1;
		display: flex;
		flex-direction: column;
    gap: 1px;
	}

	.upgrade-text strong { 
    font-size: 10px; 
    font-weight: 800;
    color: var(--color-warning-dark); 
    text-transform: uppercase;
  }
	.upgrade-text span { 
    font-size: 9px; 
    font-weight: 600;
    color: var(--chv-color-neutral-500); 
  }

	.posture-nominal {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		color: var(--color-success);
		font-size: 10px;
		font-weight: 800;
    text-transform: uppercase;
	}

	.action-list {
		list-style: none;
		padding: 0;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}

	.action-item {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		padding: 0.75rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.action-desc {
		font-size: 10px;
		font-weight: 700;
    color: var(--chv-color-neutral-800);
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
		padding: 1.5rem;
		text-align: center;
    text-transform: uppercase;
	}

	.spin-slow {
		animation: spin 3s linear infinite;
	}

	@keyframes spin {
		from { transform: rotate(0deg); }
		to { transform: rotate(360deg); }
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}
</style>
