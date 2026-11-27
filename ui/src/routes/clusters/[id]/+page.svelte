<script lang="ts">
import ResourceDetailHeader from '#lib/components/shell/ResourceDetailHeader.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import { 
		Blocks, AlertTriangle, Server, 
		History, ShieldCheck, Gauge
	} from 'lucide-svelte';
	import type { PageData } from './$types';
	import type { ShellTone } from '#lib/shell/app-shell.ts';

	let { data }: { data: PageData } = $props();

	const detail = $derived(data.detail);
	const summary = $derived(detail.summary);

	function normalizeTone(status: string): ShellTone {
		const s = status.toLowerCase();
		if (['healthy', 'ready', 'active', 'online'].includes(s)) return 'healthy';
		if (['warning', 'maintenance', 'degraded'].includes(s)) return 'warning';
		if (['failed', 'error', 'critical', 'offline'].includes(s)) return 'failed';
		return 'unknown';
	}

	const basicProps = $derived([
		{ label: 'Datacenter', value: summary.datacenter },
		{ label: 'Fabric ID', value: summary.clusterId },
		{ label: 'Managed Nodes', value: String(summary.nodeCount) },
		{ label: 'Version Skew', value: summary.versionSkew ? 'NON_UNIFORM' : 'UNIFORM' }
	]);
</script>

<div class="inventory-page">
	{#if detail.state === 'error'}
		<ErrorState title="Fabric record unreachable" description="Failed to retrieve aggregate cluster metrics from the control plane." />
	{:else}
		<ResourceDetailHeader
			title={summary.name}
			eyebrow="COMPUTE_FABRIC // {summary.clusterId}"
			statusLabel={summary.state}
			tone={normalizeTone(summary.state)}
			parentLabel="Clusters"
			parentHref="/clusters"
		>
		</ResourceDetailHeader>

		<div class="inventory-metrics">
			<CompactMetricCard 
				label="Aggregate CPU" 
				value="{summary.cpuPercent}%" 
				color={summary.cpuPercent > 80 ? 'warning' : 'primary'} 
			/>
			<CompactMetricCard 
				label="Aggregate RAM" 
				value="{summary.memoryPercent}%" 
				color={summary.memoryPercent > 80 ? 'warning' : 'primary'} 
			/>
			<CompactMetricCard 
				label="Fabric Storage" 
				value="{summary.storagePercent}%" 
				color={summary.storagePercent > 80 ? 'warning' : 'primary'} 
			/>
			<CompactMetricCard 
				label="Anomaly Count" 
				value={summary.alerts} 
				color={summary.alerts > 0 ? 'warning' : 'neutral'} 
			/>
		</div>

		<main class="inventory-main">
			<div class="detail-content">
				<SectionCard title="System Parameters" icon={Blocks}>
					<PropertyGrid properties={basicProps} columns={2} />
				</SectionCard>

				<SectionCard title="Resource Pressure Audit" icon={Gauge}>
					<div class="capacity-audit">
						<div class="audit-row">
							<div class="audit-info">
								<span class="label">CPU_CLUSTER_PRESSURE</span>
								<span class="val">{summary.cpuPercent}%</span>
							</div>
							<div class="audit-bar-track">
								<div class="audit-bar-fill" style="width: {summary.cpuPercent}%" class:high={summary.cpuPercent > 80}></div>
							</div>
						</div>
						<div class="audit-row">
							<div class="audit-info">
								<span class="label">MEMORY_CLUSTER_PRESSURE</span>
								<span class="val">{summary.memoryPercent}%</span>
							</div>
							<div class="audit-bar-track">
								<div class="audit-bar-fill" style="width: {summary.memoryPercent}%" class:high={summary.memoryPercent > 80}></div>
							</div>
						</div>
						<div class="audit-row">
							<div class="audit-info">
								<span class="label">STORAGE_GRID_PRESSURE</span>
								<span class="val">{summary.storagePercent}%</span>
							</div>
							<div class="audit-bar-track">
								<div class="audit-bar-fill" style="width: {summary.storagePercent}%" class:high={summary.storagePercent > 80}></div>
							</div>
						</div>
					</div>
				</SectionCard>

				<SectionCard title="Async Reporting" icon={Server}>
					<p class="empty-hint">Workload counts and health are not reported by the cluster list API.</p>
				</SectionCard>
			</div>

			<aside class="support-area">
				<SectionCard title="Operational Sync" icon={History} badgeLabel={String(summary.activeTasks)}>
					{#if summary.activeTasks === 0}
						<p class="empty-hint">No active mutation tasks for this cluster fabric.</p>
					{:else}
						<div class="mini-activity">
							<p>Multiple concurrent operations detected.</p>
							<a href={`/tasks?query=${encodeURIComponent(summary.clusterId)}`} class="view-link">OPEN_TASK_AUDIT</a>
						</div>
					{/if}
				</SectionCard>

				<SectionCard title="Health Propagation" icon={AlertTriangle} badgeLabel={String(summary.alerts)} badgeTone={summary.alerts > 0 ? 'warning' : 'neutral'}>
					{#if summary.alerts === 0}
						<div class="safety-sign">
							<ShieldCheck size={16} />
							<span>NO ACTIVE ALERTS</span>
						</div>
					{:else}
						<div class="mini-activity">
							<p>{summary.alerts} active anomalies requiring inspection.</p>
							<a href={`/events?query=${encodeURIComponent(summary.clusterId)}`} class="view-link">OPEN_INCIDENT_LOG</a>
						</div>
					{/if}
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

	.detail-content {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.support-area {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.capacity-audit {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.audit-row {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.audit-info {
		display: flex;
		justify-content: space-between;
		font-size: 10px;
		font-weight: 800;
	}

	.audit-info .label {
		color: var(--chv-color-neutral-500);
	}

	.audit-info .val {
		color: var(--chv-color-neutral-900);
		font-family: var(--chv-font-mono);
	}

	.audit-bar-track {
		height: 3px;
		background: var(--bg-surface-muted);
		border-radius: 2px;
		overflow: hidden;
	}

	.audit-bar-fill {
		height: 100%;
		background: var(--color-primary);
		transition: width 0.3s ease;
	}

	.audit-bar-fill.high {
		background: var(--color-warning);
	}




	.mini-activity {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.mini-activity p {
		font-size: 11px;
		color: var(--chv-color-neutral-600);
		margin: 0;
	}

	.view-link {
		font-size: 9px;
		font-weight: 800;
		text-transform: uppercase;
		text-decoration: none;
		color: var(--color-primary);
	}

	.safety-sign {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.5rem;
		background: rgba(var(--color-success-rgb), 0.1);
		color: var(--color-success);
		font-size: 10px;
		font-weight: 800;
		border-radius: var(--radius-xs);
	}



	.empty-hint {
		font-size: 11px;
		color: var(--chv-color-neutral-400);
		padding: 1rem 0;
		text-align: center;
	}

	@media (max-width: 1200px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}




	.mini-activity {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}

	.mini-activity p {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-secondary);
		margin: 0;
	}

	.view-link {
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		text-decoration: none;
		color: var(--shell-accent);
	}

	.empty-hint {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		text-align: center;
		padding: 1rem 0;
	}

</style>
