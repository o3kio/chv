<script lang="ts">
	import { selection } from '#lib/stores/selection.svelte.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import { getVm } from '#lib/bff/vms.ts';
	import { getNode } from '#lib/bff/nodes.ts';
	import type { VmSummary, NodeSummary } from '#lib/bff/types.ts';
	import { fade } from 'svelte/transition';
	import { ChevronRight, Loader2 } from 'lucide-svelte';
	import InspectorHeader from './InspectorHeader.svelte';
	import EntityIdentity from './EntityIdentity.svelte';
	import PostureCard from './PostureCard.svelte';
	import MetricBars from './MetricBars.svelte';
	import PropertyMesh from './PropertyMesh.svelte';
	import MutationControls from './MutationControls.svelte';
	import InspectorEmptyState from './InspectorEmptyState.svelte';

	const active = $derived(selection.active);
	let details = $state<any>(null);
	let isLoading = $state(false);

	$effect(() => {
		if (active.id) {
			fetchDetails(active.type, active.id);
		} else {
			details = null;
		}
	});

	function normalizeNodeStatus(state: string): string {
		const s = state.toLowerCase();
		if (s.includes('ready') || s.includes('online') || s.includes('active')) return 'online';
		if (s.includes('error') || s.includes('fail')) return 'error';
		if (s.includes('maint')) return 'maintenance';
		return 'offline';
	}

	function normalizeVmState(state: string): string {
		return state.toLowerCase();
	}

	async function fetchDetails(type: string, id: string) {
		const token = getStoredToken();
		if (!token) return;

		isLoading = true;

		try {
			if (type === 'node') {
				const res = await getNode({ node_id: id }, token);
				const summary = res.summary as NodeSummary & { architecture?: string; provider_type?: string };
				details = {
					...summary,
					id: summary.node_id,
					status: normalizeNodeStatus(summary.state),
					architecture: summary.architecture || 'x86_64',
					provider_type: summary.provider_type || 'LOCAL_HOST'
				};
			} else if (type === 'vm') {
				const res = await getVm({ vm_id: id }, token);
				const summary = res.summary as VmSummary & { architecture?: string; provider_type?: string };
				details = {
					...summary,
					id: summary.vm_id,
					actual_state: normalizeVmState(summary.power_state),
					status: normalizeVmState(summary.power_state),
					architecture: summary.architecture || 'x86_64',
					provider_type: summary.provider_type || 'LOCAL_HOST'
				};
			}
		} catch (err) {
			// TODO: integrate structured logger instead of console
			// eslint-disable-next-line no-console
			console.error('Failed to fetch inspector details:', err);
			details = null;
		} finally {
			isLoading = false;
		}
	}
</script>

<div class="flex flex-col h-full bg-(--bg-surface)">
	<InspectorHeader />

	{#if active.id}
		<div class="flex-1 p-5 flex flex-col gap-6 overflow-y-auto" in:fade={{ duration: 100 }}>
			<EntityIdentity type={active.type} label={active.label ?? ''} id={active.id ?? ''} />

			{#if isLoading}
				<div class="py-12 flex flex-col items-center gap-3 text-[10px] font-extrabold text-neutral-500">
					<Loader2 size={16} class="animate-spin" />
					<span>COLLECTING_TELEMETRY...</span>
				</div>
			{:else if details}
				<div class="flex flex-col gap-6">
					<div class="flex flex-col gap-2.5">
						<div class="text-[9px] font-extrabold text-neutral-500 uppercase tracking-widest">Operational Posture</div>
						<PostureCard status={details.status} actual_state={details.actual_state} />
					</div>

					<div class="flex flex-col gap-2.5">
						<div class="text-[9px] font-extrabold text-neutral-500 uppercase tracking-widest">System Pulse</div>
						<MetricBars cpuUsage={0} memoryUsagePercent={0} />
					</div>

					<div class="flex flex-col gap-2.5">
						<div class="text-[9px] font-extrabold text-neutral-500 uppercase tracking-widest">Properties</div>
						<PropertyMesh
							architecture={details.architecture}
							providerType={details.provider_type}
						/>
					</div>

					<div class="flex flex-col gap-2.5">
						<div class="text-[9px] font-extrabold text-neutral-500 uppercase tracking-widest">Actions</div>
						<MutationControls />
					</div>

					<a href={`/${active.type}s/${active.id}`} class="flex items-center justify-between p-3 bg-[rgba(var(--color-primary-rgb),0.1)] text-primary no-underline text-[10px] font-extrabold rounded-xs mt-2 hover:bg-[rgba(var(--color-primary-rgb),0.15)]">
						<span>FULL_INSPECTION_DETAIL</span>
						<ChevronRight size={14} />
					</a>
				</div>
			{/if}
		</div>
	{:else}
		<InspectorEmptyState />
	{/if}
</div>
