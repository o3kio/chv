<script lang="ts">
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';
	import TaskTimeline from '#lib/components/shell/TaskTimeline.svelte';
	import VMMetricsWidget from './VMMetricsWidget.svelte';
	import { Database, Network, Activity } from 'lucide-svelte';
	import type { ShellTone } from '#lib/shell/app-shell.ts';

	interface Props {
		powerState: string;
		health: string;
		cpu: string;
		memory: string;
		volumes?: any[];
		nics?: any[];
		recentTasks?: any[];
	}

	let {
		powerState,
		health,
		cpu,
		memory,
		volumes = [],
		nics = [],
		recentTasks = []
	}: Props = $props();

	function normalizeTone(status: string): ShellTone {
		const s = status.toLowerCase();
		if (['running', 'healthy', 'ready', 'active', 'online'].includes(s)) return 'healthy';
		if (['warning', 'maintenance', 'starting', 'stopping', 'paused', 'rebooting'].includes(s)) return 'warning';
		if (['failed', 'error', 'critical', 'crashed', 'deleting'].includes(s)) return 'failed';
		return 'unknown';
	}

	const volumeColumns = [
		{ key: 'name', label: 'Volume Identity' },
		{ key: 'size', label: 'Density', align: 'right' as const },
		{ key: 'device_name', label: 'Device Path' },
		{ key: 'health', label: 'Integrity', align: 'center' as const }
	];

	const nicColumns = [
		{ key: 'network_name', label: 'Fabric Registry' },
		{ key: 'ip_address', label: 'Primary IP' },
		{ key: 'mac_address', label: 'L2 Identity' },
		{ key: 'addressing_mode', label: 'DHCP/STA', align: 'center' as const }
	];

	const volumeRows = $derived(
		volumes.map(v => ({
			...v,
			health: { label: v.health, tone: normalizeTone(v.health) }
		}))
	);

	const nicRows = $derived(
		nics.map(n => ({
			...n,
			ip_address: n.ip_address || 'UNASSIGNED',
			addressing_mode: {
				label: n.addressing_mode === 'internal' ? 'DHCP' : 'STATIC',
				tone: n.addressing_mode === 'internal' ? 'healthy' : 'warning'
			}
		}))
	);

	const timelineTasks = $derived(
		recentTasks.map(t => ({
			...t,
			tone: normalizeTone(t.status)
		}))
	);
</script>

<div class="summary-top">
	<SectionCard title="Workload Posture" icon={Activity}>
		<PropertyGrid
			properties={[
				{ label: 'Power Matrix', value: powerState },
				{ label: 'Safety Integrity', value: health },
				{ label: 'Core Alloc', value: cpu },
				{ label: 'RAM Reserv', value: memory }
			]}
			columns={4}
		/>
	</SectionCard>
</div>

<div class="vital-metrics">
	<VMMetricsWidget vms={{
		total: 1,
		running: powerState.toLowerCase() === 'running' ? 1 : 0,
		stopped: powerState.toLowerCase() === 'stopped' ? 1 : 0,
		error: health.toLowerCase() === 'error' ? 1 : 0
	}} />
</div>

<SectionCard title="Storage Fabric" icon={Database} badgeLabel={String(volumes.length)}>
	{#if volumes.length === 0}
		<p class="empty-hint">No storage volumes mapped to this workload fabric.</p>
	{:else}
		<InventoryTable
			columns={volumeColumns}
			rows={volumeRows}
			rowHref={(row) => `/volumes/${row.volume_id}`}
		>
			{#snippet cell({ column, row }: { column: any; row: Record<string, unknown> })}
				{#if column.key === 'name'}
					<span class="workload-name">{row.name}</span>
				{:else if column.key === 'health'}
					<StatusBadge label={(row.health as any).label} tone={(row.health as any).tone} />
				{:else}
					<span class="cell-text">{row[column.key]}</span>
				{/if}
			{/snippet}
		</InventoryTable>
	{/if}
</SectionCard>

<SectionCard title="Network Mesh" icon={Network} badgeLabel={String(nics.length)}>
	{#if nics.length === 0}
		<p class="empty-hint">No L2 interfaces defined for this virtual entity.</p>
	{:else}
		<InventoryTable
			columns={nicColumns}
			rows={nicRows}
		>
			{#snippet cell({ column, row }: { column: any; row: Record<string, unknown> })}
				{#if column.key === 'addressing_mode'}
					<StatusBadge label={(row.addressing_mode as any).label} tone={(row.addressing_mode as any).tone} />
				{:else}
					<span class="cell-text">{row[column.key]}</span>
				{/if}
			{/snippet}
		</InventoryTable>
	{/if}
</SectionCard>

<SectionCard title="Operational History" icon={Activity}>
	<TaskTimeline tasks={timelineTasks} />
</SectionCard>

<style>
	.summary-top,
	.vital-metrics {
		display: flex;
		flex-direction: column;
	}

	.empty-hint {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		text-align: center;
		padding: 1rem 0;
	}
</style>
