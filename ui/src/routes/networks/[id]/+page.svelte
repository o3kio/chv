<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import type { PageData } from './$types';
	import { getPageDefinition } from '#lib/shell/app-shell.ts';
	import type { ShellTone } from '#lib/shell/app-shell.ts';
	import ResourceDetailHeader from '#lib/components/shell/ResourceDetailHeader.svelte';
	import PropertyGrid from '#lib/components/shell/PropertyGrid.svelte';
	import ActionStrip from '#lib/components/shell/ActionStrip.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import InventoryTable from '#lib/components/shell/InventoryTable.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import { Shield, ShieldAlert, Network, Box, Activity, Info, AlertTriangle, Pencil } from 'lucide-svelte';
	import CreateNetworkModal from '#lib/components/networks/CreateNetworkModal.svelte';

	let { data }: { data: PageData } = $props();

	const detail = $derived(data.detail);

	function normalizeTone(status: string): ShellTone {
		const s = status.toLowerCase();
		if (['healthy', 'ready', 'active', 'online'].includes(s)) return 'healthy';
		if (['warning', 'maintenance', 'starting', 'stopping', 'paused', 'nat'].includes(s)) return 'warning';
		if (['degraded', 'offline', 'public'].includes(s)) return 'degraded';
		if (['failed', 'error', 'critical', 'crashed'].includes(s)) return 'failed';
		return 'unknown';
	}

	const postureProps = $derived([
		{ label: 'Infrastructure Health', value: detail.health, tone: normalizeTone(detail.health) as any },
		{ label: 'Exposure Status', value: detail.exposure, tone: normalizeTone(detail.exposure) as any },
		{ label: 'CIDR', value: detail.cidr },
		{ label: 'Gateway', value: detail.gateway }
	]);

	const policyProps = $derived([
		{ label: 'Active Policy', value: detail.policy },
		{ label: 'Scope', value: detail.scope },
		{ label: 'Created', value: new Date(detail.created_at).toLocaleDateString() }
	]);

	const vmColumns = [
		{ key: 'display_name', label: 'VM' },
		{ key: 'ip_address', label: 'IP Address' },
		{ key: 'mac_address', label: 'MAC Address' },
		{ key: 'runtime_status', label: 'State' },
		{ key: 'addressing_mode', label: 'Addressing' }
	];

	function addressingTone(mode: string): ShellTone {
		if (mode === 'internal') return 'healthy';
		if (mode === 'external') return 'warning';
		return 'unknown';
	}

	function addressingLabel(mode: string): string {
		if (mode === 'internal') return 'DHCP';
		if (mode === 'external') return 'External';
		return 'Static';
	}

	function vmStatusTone(status: string): ShellTone {
		const s = status.toLowerCase();
		if (s === 'running') return 'healthy';
		if (s === 'stopped') return 'unknown';
		if (s.includes('fail') || s.includes('error')) return 'failed';
		return 'unknown';
	}

	let showEditModal = $state(false);

	const vmRows = $derived(detail.attached_vms.map(v => ({
		...v,
		ip_address: v.ip_address || '—',
		mac_address: v.mac_address || '—',
		runtime_status: { label: v.runtime_status || 'unknown', tone: vmStatusTone(v.runtime_status || '') },
		addressing_mode: { label: addressingLabel(detail.ipam_mode), tone: addressingTone(detail.ipam_mode) }
	})));
</script>

<div class="resource-detail">
	{#if detail.state === 'error'}
		<ErrorState title={detail.title ?? 'Network Detail Unavailable'} description={detail.description ?? 'The SDN controller could not provide synchronization data.'} />
	{:else}
		<ResourceDetailHeader
			title={detail.name}
			eyebrow={detail.scope}
			statusLabel={detail.exposure}
			tone={normalizeTone(detail.exposure)}
			parentLabel="Networks"
			parentHref="/networks"
			description="Defined software network segment."
		>
			{#snippet actions()}
				<ActionStrip>
					{#if detail.exposure === 'public'}
						<Button variant="secondary" size="sm" disabled title="Not available in this release">
							<ShieldAlert size={14} />
							Withdraw Exposure
						</Button>
					{:else}
						<Button variant="secondary" size="sm" disabled title="Not available in this release">
							<Shield size={14} />
							Expose Publicly
						</Button>
					{/if}
					<Button variant="secondary" size="sm" disabled title="Not available in this release">
						<Activity size={14} />
						Edit Policy
					</Button>
					<Button variant="secondary" size="sm" onclick={() => showEditModal = true}>
						<Pencil size={14} />
						Edit
					</Button>
			</ActionStrip>
			{/snippet}
		</ResourceDetailHeader>

		{#if detail.is_default}
			<div class="default-banner">
				<Info size={14} />
				<span>This network is the default for dev-install workloads.</span>
			</div>
		{/if}

		<main class="detail-grid">
			<section class="detail-main-span">
				<div class="summary-top">
					<SectionCard title="Network Posture" icon={Activity}>
						<PropertyGrid properties={[...postureProps, ...policyProps]} columns={4} />
					</SectionCard>
				</div>

				<div class="detail-sections">
					<SectionCard title="Connected Workloads" icon={Box} badgeLabel={String(detail.attached_vms.length)}>
						{#if detail.attached_vms.length === 0}
							<p class="empty-hint">No virtual machines currently attached to this subnet.</p>
						{:else}
							<InventoryTable
								columns={vmColumns}
								rows={vmRows}
								rowHref={(row) => `/vms/${row.vm_id}`}
							/>
						{/if}
					</SectionCard>

					<SectionCard title="Policy History" icon={Activity}>
						{#if detail.last_task}
							<p class="last-activity">Last activity: {detail.last_task}</p>
						{:else}
							<p class="empty-hint">No events in the last 24 hours.</p>
						{/if}
					</SectionCard>
				</div>
			</section>

			<CreateNetworkModal
				bind:open={showEditModal}
				editMode={true}
				network={detail}
				onSuccess={() => window.location.reload()}
			/>

			<aside class="detail-side-span">
				<SectionCard title="L3 Configuration" icon={Network}>
					<PropertyGrid
						columns={1}
						properties={[
							{ label: 'Subnet ID', value: detail.network_id },
							{ label: 'Gateway IP', value: detail.gateway },
							{ label: 'DHCP', value: detail.dhcp_enabled ? 'Enabled' : 'Disabled' },
							{ label: 'IPAM Mode', value: detail.ipam_mode }
						]}
					/>
				</SectionCard>

				<SectionCard title="Ingress Audit" icon={AlertTriangle}>
					{#if detail.exposure === 'public'}
						<div class="alert-box tone-warning">
							<span class="alert-label">Public Exposure Active</span>
							<p class="alert-desc">This network is reachable from external gateways. Ensure security policies are restricted.</p>
						</div>
					{:else}
						<p class="empty-hint">Network is private. Only peered cluster traffic allowed.</p>
					{/if}
				</SectionCard>
			</aside>
		</main>
	{/if}
</div>

<style>
	.resource-detail {
		display: flex;
		flex-direction: column;
	}

	.default-banner {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.5rem 0.75rem;
		margin-bottom: 1rem;
		background: var(--shell-surface-muted);
		border: 1px solid var(--shell-line);
		border-radius: 0.35rem;
		font-size: var(--chv-text-sm);
		color: var(--shell-text-secondary);
	}

	.detail-grid {
		display: grid;
		grid-template-columns: 1fr 300px;
		gap: 1rem;
		align-items: start;
	}

	.detail-main-span {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.detail-sections {
		display: grid;
		grid-template-columns: 1fr;
		gap: 1rem;
	}

	.detail-side-span {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.empty-hint {
		font-size: var(--chv-text-xs);
		color: var(--shell-text-muted);
		text-align: center;
		padding: 1rem 0;
	}

	.last-activity {
		font-size: var(--chv-text-sm);
		color: var(--shell-text-secondary);
		padding: 0.5rem 0;
	}

	.alert-box {
		padding: 0.75rem;
		border-radius: 0.25rem;
		background: var(--color-warning-light);
		border: 1px solid var(--color-warning-dark);
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.alert-label {
		font-weight: 700;
		font-size: var(--chv-text-xs);
		color: var(--color-warning-dark);
	}

	.alert-desc {
		font-size: var(--chv-text-xs);
		color: var(--color-warning-dark);
		margin: 0;
	}

	@media (max-width: 1200px) {
		.detail-grid {
			grid-template-columns: 1fr;
		}
	}
</style>
