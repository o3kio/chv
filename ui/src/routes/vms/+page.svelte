<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import type { PageData } from './$types';
	import InventoryListPage from '#lib/components/shell/InventoryListPage.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import CreateVMModal from '#lib/components/vms/CreateVMModal.svelte';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import { mapPowerTone } from '#lib/shell/power-tone.ts';
	import { Plus, Activity, AlertCircle, ShieldCheck } from 'lucide-svelte';
	import { goto } from '$app/navigation';
	import { page as appPage } from '$app/state';

	let { data }: { data: PageData } = $props();

	let modalOpen = $state(false);
	const model = $derived(data.vms);
	const items = $derived(model.items);

	const pageDef = getPageDefinition('/vms');

	const filters = [
		{ key: 'query', label: 'Search', type: 'text' as const, placeholder: 'Name or node...' },
		{
			key: 'powerState',
			label: 'Power',
			type: 'select' as const,
			options: [
				{ value: 'running', label: 'Running' },
				{ value: 'stopped', label: 'Stopped' },
				{ value: 'paused', label: 'Paused' },
				{ value: 'crashed', label: 'Crashed' }
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

	function handleFilterChange(key: string, value: unknown) {
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
		{ key: 'name', label: 'Workload' },
		{ key: 'node_id', label: 'Host Node' },
		{ key: 'power_state', label: 'State', align: 'center' as const },
		{ key: 'health', label: 'Posture' },
		{ key: 'cpu', label: 'CPU', align: 'right' as const },
		{ key: 'memory', label: 'Memory', align: 'right' as const },
		{ key: 'last_task', label: 'Recent Operation' }
	];

	function mapHealthTone(health: string): string {
		switch (health) {
			case 'healthy': return 'healthy';
			case 'warning': return 'warning';
			case 'critical': return 'failed';
			default: return 'neutral';
		}
	}

	const tableRows = $derived(items.map(item => ({
		...item,
		power_state: { label: item.power_state, tone: mapPowerTone(item.power_state) },
		health: { label: item.health, tone: mapHealthTone(item.health) }
	})));

	const attentionVms = $derived(items.filter(v => v.health !== 'healthy' || (v.alerts ?? 0) > 0).slice(0, 3));

	const metrics = $derived([
		{ label: 'Catalog', value: items.length, color: 'neutral' as const },
		{ label: 'Running', value: items.filter(v => v.power_state.toLowerCase() === 'running').length, trend: +1, color: 'primary' as const },
		{ label: 'Attention', value: items.filter(v => v.health !== 'healthy').length, color: items.filter(v => v.health !== 'healthy').length > 0 ? 'warning' as const : 'neutral' as const }
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
	emptyTitle="Discovery Filter Active"
	emptyDescription="No workloads match the current projection."
	emptyHint="Refine your search parameters or check archived objects."
	{columns}
	rows={tableRows}
	rowHref={(row) => `/vms/${row.vm_id}`}
>
	{#snippet headerActions()}
		<Button variant="primary" onclick={() => (modalOpen = true)}>
			<Plus size={14} />
			Deploy Workload
		</Button>
	{/snippet}

	{#snippet sidebar()}
		<SectionCard title="Dispatch Cue" icon={Activity} badgeLabel={items.length > 0 ? 'Live' : 'Idle'}>
			<div class="dispatch-cue">
				<p>Use this page as the operator queue, not a vanity dashboard.</p>
				<span>
					{#if items.length > 0}
						{items.filter((vm) => vm.power_state.toLowerCase() === 'running').length} workloads are currently active across the indexed fleet.
					{:else}
						No workloads are currently indexed.
					{/if}
				</span>
			</div>
		</SectionCard>

		<SectionCard title="Anomaly Detection" icon={AlertCircle} badgeLabel={String(attentionVms.length)}>
			{#if attentionVms.length === 0}
				<p class="empty-hint">Signals nominal. Posture is stable.</p>
			{:else}
				<ul class="attention-list">
					{#each attentionVms as vm}
						<li>
							<a href={`/vms/${vm.vm_id}`} class="attention-card">
								<div class="attention-card__main">
									<span class="res-name">{vm.name}</span>
									<span class="res-issue">{vm.alerts || 0} signals / {vm.health}</span>
								</div>
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</SectionCard>

		<SectionCard title="Provisioning Log" icon={ShieldCheck}>
			<ul class="task-list">
				<li class="task-item">
					<span class="task-label">Replication Engine</span>
					<span class="task-time">Online</span>
				</li>
				<li class="task-item">
					<span class="task-label">Migration Target Sync</span>
					<span class="task-time">Active</span>
				</li>
			</ul>
		</SectionCard>
	{/snippet}
</InventoryListPage>

<CreateVMModal
		bind:open={modalOpen}
		onSuccess={() => {}}
	/>

<style>
	.dispatch-cue {
		display: flex;
		flex-direction: column;
		gap: 0.45rem;
		padding: 0.1rem 0;
	}

	.dispatch-cue p {
		margin: 0;
		font-size: var(--chv-text-sm);
		line-height: 1.45;
		color: var(--shell-text);
	}

	.dispatch-cue span {
		font-size: var(--chv-text-xs);
		line-height: 1.5;
		color: var(--shell-text-muted);
	}

	:global(.inventory-page) {
		gap: 0.625rem;
	}

	:global(.inventory-page .section-header) {
		gap: 0.5rem;
	}

	:global(.inventory-page .section-header h1) {
		font-size: clamp(2.1rem, 3vw, 2.65rem);
	}

	:global(.inventory-page .section-header__meta) {
		gap: 0.65rem 1rem;
	}

	:global(.inventory-page .section-header__meta p) {
		max-width: 38rem;
		line-height: 1.5;
	}

	:global(.inventory-metrics) {
		grid-template-columns: repeat(3, minmax(0, 1fr));
	}

	:global(.inventory-main) {
		grid-template-columns: minmax(0, 1.7fr) minmax(17rem, 18.5rem);
		gap: 0.75rem;
	}

	:global(.inventory-controls) {
		border-radius: var(--radius-sm);
	}

	:global(.inventory-table-area) {
		min-width: 0;
	}

	@media (max-width: 900px) {
		:global(.inventory-metrics) {
			grid-template-columns: 1fr;
		}
	}
</style>
