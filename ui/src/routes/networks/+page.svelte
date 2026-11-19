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
	import CreateNetworkModal from '#lib/components/networks/CreateNetworkModal.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { ShellTone } from '#lib/shell/app-shell.ts';
	import type { PageData } from './$types';
	import { Plus, Shield, Globe, Lock } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page as appPage } from '$app/state';

	let { data }: { data: PageData } = $props();

	let createOpen = $state(false);

	const model = $derived(data.networks);
	const items = $derived(model.items);

	const filters = [
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Name/Scope...' },
		{ 
			key: 'exposure', 
			label: 'Exposure', 
			type: 'select' as const, 
			options: [
				{ value: 'private', label: 'Private' },
				{ value: 'nat', label: 'NAT' },
				{ value: 'public', label: 'Public' }
			] 
		},
		{
			key: 'health',
			label: 'Health',
			type: 'select' as const,
			options: [
				{ value: 'healthy', label: 'Healthy' },
				{ value: 'warning', label: 'Warning' },
				{ value: 'degraded', label: 'Degraded' }
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

	function handleClearFilters() {
		goto(appPage.url.pathname);
	}

	const columns = [
		{ key: 'name', label: 'Network Fabric' },
		{ key: 'scope', label: 'Scope' },
		{ key: 'health', label: 'Fabric Posture' },
		{ key: 'attached_vms', label: 'Workloads', align: 'center' as const },
		{ key: 'exposure', label: 'Exposure', align: 'center' as const },
		{ key: 'policy', label: 'Policy Index' }
	];

	function mapHealthTone(health: string): any {
		switch (health) {
			case 'healthy': return 'healthy';
			case 'warning': return 'warning';
			case 'degraded': return 'degraded';
			default: return 'neutral';
		}
	}

	function mapExposureTone(exposure: string): any {
		switch (exposure) {
			case 'public': return 'warning';
			case 'nat': return 'neutral';
			case 'private': return 'healthy';
			default: return 'neutral';
		}
	}

	const tableRows = $derived(items.map(item => ({
		...item,
		health: { label: item.health, tone: mapHealthTone(item.health) },
		exposure: { label: item.exposure, tone: mapExposureTone(item.exposure) }
	})));

	function isBadge(val: any): val is { label: string; tone: ShellTone } {
		return val && typeof val === 'object' && 'tone' in val && 'label' in val;
	}

	const vulnerableNetworks = $derived(items.filter(n => n.exposure === 'public').slice(0, 3));
	const pageDef = getPageDefinition('/networks');
</script>

<div class="inventory-page">
	<PageHeaderWithAction page={pageDef}>
		{#snippet actions()}
			<Button variant="primary" onclick={() => createOpen = true}>
				<Plus size={14} />
				Define Fabric
			</Button>
		{/snippet}
	</PageHeaderWithAction>

	<div class="inventory-metrics">
		<CompactMetricCard 
			label="Total Segments" 
			value={items.length} 
			color="neutral"
		/>
		<CompactMetricCard 
			label="External Gateways" 
			value={items.filter(n => n.exposure === 'public').length} 
			color={items.filter(n => n.exposure === 'public').length > 0 ? 'warning' : 'neutral'}
		/>
		<CompactMetricCard 
			label="Fabric Errors" 
			value={items.filter(n => n.health !== 'healthy').length} 
			color={items.filter(n => n.health !== 'healthy').length > 0 ? 'danger' : 'neutral'}
		/>
	</div>

	<div class="inventory-controls">
		<FilterBar 
			{filters} 
			activeFilters={model.filters.current} 
			onFilterChange={handleFilterChange}
			onClearAll={handleClearFilters}
		/>
	</div>

	<main class="inventory-main">
		<section class="inventory-table-area">
			{#if model.state === 'error'}
				<ErrorState
					description={model.errorMessage ?? 'The control plane responded with an error or is unreachable.'}
				/>
			{:else if model.state === 'empty'}
				<EmptyInfrastructureState 
					title="No fabrics detected"
					description="Adjust your search criteria or define a new network segment."
					hint="Fabrics can be VXLAN-backed for multi-host isolation."
				/>
			{:else}
				<InventoryTable
					{columns}
					rows={tableRows}
					rowHref={(row) => `/networks/${row.network_id}`}
				>
					{#snippet cell({ column, row })}
						{@const val = (row as Record<string, unknown>)[column.key]}
						{#if column.key === 'name'}
							<div class="fabric-identity">
								<span class="fabric-name">{row.name}</span>
								{#if row.is_default}
									<span class="fabric-tag">CORE</span>
								{/if}
							</div>
						{:else if isBadge(val)}
							<StatusBadge label={val.label} tone={val.tone} />
						{:else}
							<span class="cell-text">{val}</span>
						{/if}
					{/snippet}
				</InventoryTable>
			{/if}
		</section>

		<aside class="support-area">
			<SectionCard title="Exposure Audit" icon={Shield} badgeLabel={String(vulnerableNetworks.length)}>
				{#if vulnerableNetworks.length === 0}
					<p class="empty-hint">All ingress points are isolated within VPC/NAT.</p>
				{:else}
					<ul class="attention-list">
						{#each vulnerableNetworks as net}
							<li>
								<div class="attention-card">
									<div class="attention-card__main">
										<span class="res-name">{net.name}</span>
										<span class="res-issue">Public Traffic Active</span>
									</div>
									<Globe size={12} class="text-warning" />
								</div>
							</li>
						{/each}
					</ul>
				{/if}
			</SectionCard>

			<SectionCard title="Fabric Topology" icon={Lock}>
				<ul class="task-list">
					<li class="task-item">
						<span class="task-label">SDN Controller</span>
						<span class="task-time">Synchronized</span>
					</li>
					<li class="task-item">
						<span class="task-label">BGP Peer Status</span>
						<span class="task-time">Established</span>
					</li>
				</ul>
			</SectionCard>
		</aside>
	</main>
</div>

<CreateNetworkModal
		bind:open={createOpen}
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

	.fabric-identity {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.fabric-name {
		font-weight: 700;
		color: var(--chv-color-neutral-900);
	}

	.fabric-tag {
		font-size: 8px;
		font-weight: 800;
		color: #ffffff;
		background: var(--color-primary);
		padding: 1px 3px;
		border-radius: 2px;
	}

	.empty-hint {
		font-size: 11px;
		color: var(--chv-color-neutral-400);
		padding: 1rem;
		text-align: center;
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
	}

	.attention-card__main {
		display: flex;
		flex-direction: column;
	}

	.res-name {
		font-size: 11px;
		font-weight: 700;
	}

	.res-issue {
		font-size: 9px;
		color: var(--color-warning);
		font-weight: 600;
		text-transform: uppercase;
	}

	.task-list {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		list-style: none;
		padding: 0;
		margin: 0;
	}

	.task-item {
		display: flex;
		justify-content: space-between;
		font-size: 10px;
		padding: 0.35rem 0.5rem;
		background: var(--bg-surface-muted);
		border-radius: var(--radius-xs);
	}

	.task-label {
		font-weight: 600;
		color: var(--chv-color-neutral-700);
	}

	.task-time {
		color: var(--color-success);
		font-weight: 700;
		text-transform: uppercase;
		font-size: 9px;
	}

	@media (max-width: 1100px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}
</style>
