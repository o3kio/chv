<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import FilterBar from '#lib/components/shared/FilterBar.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import CreateVolumeModal from '#lib/components/volumes/CreateVolumeModal.svelte';
	import { getStoredRole } from '#lib/api/client.ts';
	import { Plus, Database, Activity, HardDrive, ShieldAlert } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page as appPage } from '$app/state';

	let { data }: { data: PageData } = $props();

	let modalOpen = $state(false);

	// The create route is operator-or-admin tier
	// (`require_operator_or_admin` on POST /v1/volumes/create) — the
	// affordance is hidden for viewers, mirroring the route's own
	// tier (unlike the quotas page's stricter admin-only gate for
	// its write surface). Client-side hiding is cosmetic; the server
	// enforces the tier.
	let canAllocate = $state(['operator', 'admin'].includes(getStoredRole() ?? ''));

	const model = $derived(data.volumes);
	const items = $derived(model.items);

	const filters = [
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Name/VM/Node...' },
		{ 
			key: 'status', 
			label: 'Status', 
			type: 'select' as const, 
			options: [
				{ value: 'available', label: 'Available' },
				{ value: 'provisioning', label: 'Provisioning' },
				{ value: 'error', label: 'Error' }
			] 
		},
		{
			key: 'health',
			label: 'Health',
			type: 'select' as const,
			options: [
				{ value: 'healthy', label: 'Healthy' },
				{ value: 'warning', label: 'Warning' },
				{ value: 'critical', label: 'Critical' }
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
		{ key: 'name', label: 'Volume Identity' },
		{ key: 'attached_vm_name', label: 'Attachment' },
		{ key: 'node_id', label: 'Placement' },
		{ key: 'status', label: 'State' },
		{ key: 'health', label: 'IO Health' },
		{ key: 'size', label: 'Durable Size', align: 'right' as const },
		{ key: 'last_task', label: 'Last Seq', align: 'right' as const }
	];

	function mapHealthTone(health: string): any {
		switch (health) {
			case 'healthy': return 'healthy';
			case 'warning': return 'warning';
			case 'degraded': return 'degraded';
			case 'critical': return 'failed';
			default: return 'unknown';
		}
	}

	// #522 DP12: the list's State column — the tombstone render. The
	// BFF already serves `status` via the read COALESCE (desired_status
	// first, #535), but the list table never displayed it, so a
	// 'Deleting' volume was invisible on this surface. Vocabulary and
	// tones mirror the detail page's normalizeTone (with 'pending' for
	// fresh creates); 'deleting' rides the failed tone, the VM detail
	// page's vocabulary.
	function mapStatusTone(status: string): any {
		const s = status.toLowerCase();
		if (['attached', 'healthy', 'ready', 'active', 'online'].includes(s)) return 'healthy';
		if (['pending', 'warning', 'maintenance', 'attaching', 'detaching', 'resizing', 'available'].includes(s)) return 'warning';
		if (['degraded', 'offline'].includes(s)) return 'degraded';
		if (['failed', 'error', 'critical', 'deleting'].includes(s)) return 'failed';
		return 'unknown';
	}

	const tableRows = $derived(items.map(item => ({
		...item,
		status: { label: item.status, tone: mapStatusTone(item.status) },
		health: { label: item.health, tone: mapHealthTone(item.health) }
	})));

	const attentionVolumes = $derived(items.filter(v => v.health !== 'healthy').slice(0, 3));
	const pageDef = getPageDefinition('/volumes');
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={pageDef}>
		{#snippet actions()}
			{#if canAllocate}
				<Button variant="primary" onclick={() => (modalOpen = true)}>
					<Plus size={14} />
					Allocate Block
				</Button>
			{/if}
		{/snippet}
	</PageHeaderWithAction>

	<div class="inventory-metrics">
		<CompactMetricCard 
			label="Total Blocks" 
			value={items.length} 
			color="neutral"
		/>
		<CompactMetricCard 
			label="Hot Attachments" 
			value={items.filter(v => v.attached_vm_id).length} 
			color="primary"
		/>
		<CompactMetricCard 
			label="Available Pools" 
			value={items.filter(v => !v.attached_vm_id).length} 
			color="neutral"
		/>
		<CompactMetricCard 
			label="Storage IOPS" 
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
					title="No block volumes provisioned"
					description="Adjust your filters or allocate a new persistent block device."
					hint="Block volumes provide high-performance durable storage for workloads."
				/>
			{:else}
				<InventoryTable 
					{columns} 
					rows={tableRows} 
					rowHref={(row) => `/volumes/${row.volume_id}`} 
				>
					{#snippet cell({ column, row })}
						{@const val = (row as any)[column.key]}
						{#if column.key === 'name'}
							<span class="volume-name">{row.name}</span>
						{:else if typeof val === 'object' && val?.tone}
							<StatusBadge label={val.label} tone={val.tone} />
						{:else}
							<span class="cell-text">{val || '—'}</span>
						{/if}
					{/snippet}
				</InventoryTable>
			{/if}
		</section>

		<aside class="support-area">
			<SectionCard title="Storage Anomalies" icon={ShieldAlert} badgeLabel={String(attentionVolumes.length)}>
				{#if attentionVolumes.length === 0}
					<p class="empty-hint">Block devices health reported as nominal.</p>
				{:else}
					<ul class="attention-list">
						{#each attentionVolumes as vol}
							<li>
								<div class="attention-card">
									<div class="attention-card__main">
										<span class="res-name">{vol.name}</span>
										<span class="res-issue">IO latency spike detected</span>
									</div>
								</div>
							</li>
						{/each}
					</ul>
				{/if}
			</SectionCard>

			<SectionCard title="Fabric Telemetry" icon={Activity}>
				<div class="audit-summary">
					<div class="summary-row">
						<span>I/O Throughput</span>
						<span>2.4 GB/s</span>
					</div>
					<div class="summary-row">
						<span>Write Latency</span>
						<span>&lt; 0.1ms</span>
					</div>
				</div>
			</SectionCard>
		</aside>
	</main>
</div>

<CreateVolumeModal
	bind:open={modalOpen}
	onSuccess={() => {}}
/>

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

	.volume-name {
		font-weight: 800;
		color: var(--chv-color-neutral-900);
    letter-spacing: 0.02em;
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
