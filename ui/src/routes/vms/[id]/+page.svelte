<script lang="ts">
	import { browser } from '$app/env';
	import type { PageData } from './$types';
	import { getStoredToken } from '#lib/api/client.ts';
	import { getVmConsoleUrl, getVmBootLog, mutateVm, deleteVm } from '#lib/bff/vms.ts';
	import { listVmSnapshots } from '#lib/bff/snapshots.ts';
	import { listVmEvents } from '#lib/bff/events.ts';
	import type { VmSnapshotItem, InfrastructureEvent } from '#lib/bff/types.ts';
	import { toast } from '#lib/stores/toast.svelte.ts';
	import { liveState } from '#lib/stores/live-state.svelte.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import EmptyInfrastructureState from '#lib/components/shell/EmptyInfrastructureState.svelte';
	import DetailTabs from '#lib/components/shared/DetailTabs.svelte';
	import VmSnapshots from '#lib/components/vms/VmSnapshots.svelte';
	import VmDetailErrorState from '#lib/components/vms/VmDetailErrorState.svelte';
	import VmDetailSupportRail from '#lib/components/vms/VmDetailSupportRail.svelte';
	import VmMigrateModal from '#lib/components/vms/VmMigrateModal.svelte';
	import VmDetailHeader from '#lib/components/vms/VmDetailHeader.svelte';
	import VmDetailSummaryTab from '#lib/components/vms/VmDetailSummaryTab.svelte';
	import VmConsoleTab from '#lib/components/vms/VmConsoleTab.svelte';
	import VmMetricsTab from '#lib/components/vms/VmMetricsTab.svelte';
	import TargetMonitoringPanel from '#lib/components/monitoring/TargetMonitoringPanel.svelte';
	import GuestAgentCard from '#lib/components/monitoring/GuestAgentCard.svelte';
	import GuestFilesystemsCard from '#lib/components/monitoring/GuestFilesystemsCard.svelte';
	import GuestChecksCard from '#lib/components/monitoring/GuestChecksCard.svelte';
	import GuestProcessesCard from '#lib/components/monitoring/GuestProcessesCard.svelte';
	import GuestAgentEnrollDialog from '#lib/components/monitoring/GuestAgentEnrollDialog.svelte';
	import VmTasksTab from '#lib/components/vms/VmTasksTab.svelte';
	import VmBootLogTab from '#lib/components/vms/VmBootLogTab.svelte';
	import type { ShellTone } from '#lib/shell/app-shell.ts';

	let { data }: { data: PageData } = $props();

	const detail = $derived(data.detail);
	let pendingAction = $state<string | null>(null);
	let liveConsoleUrl = $state<string | undefined>(undefined);
	let liveConsoleToken = $state<string | undefined>(undefined);
	let liveConsoleExpiresAt = $state<string | undefined>(undefined);
	let VmConsoleComponent = $state<typeof import('#lib/components/vms/VmConsole.svelte').default | null>(null);
	let consoleLoading = $state(false);
	let bootLog = $state<string>('');
	let bootLogLoading = $state(false);
	let snapshots = $state<VmSnapshotItem[]>([]);
	let snapshotsLoading = $state(false);
	let snapshotsError = $state<string | null>(null);
	let events = $state<InfrastructureEvent[]>([]);
	let eventsLoading = $state(false);
	let eventsError = $state<string | null>(null);
	let supportRailOpen = $state(false);
	let migrateModalOpen = $state(false);
	let migrateSubmitting = $state(false);
	let agentEnrollOpen = $state(false);

	async function ensureVmConsole() {
		if (!browser || VmConsoleComponent) return;
		const module = await import('#lib/components/vms/VmConsole.svelte');
		VmConsoleComponent = module.default;
	}

	$effect(() => {
		if (detail.currentTab === 'console' && detail.summary.vm_id) {
			ensureVmConsole();
			consoleLoading = true;
			getVmConsoleUrl(detail.summary.vm_id, getStoredToken() ?? undefined)
				.then(res => {
					liveConsoleUrl = res.url;
					liveConsoleToken = res.token;
					liveConsoleExpiresAt = res.expires_at;
				})
				.catch(() => {
					liveConsoleUrl = undefined;
					liveConsoleToken = undefined;
					liveConsoleExpiresAt = undefined;
				})
				.finally(() => { consoleLoading = false; });
		}
	});

	$effect(() => {
		if (detail.currentTab === 'boot-log' && detail.summary.vm_id) {
			bootLogLoading = true;
			getVmBootLog(detail.summary.vm_id, getStoredToken() ?? undefined)
				.then(res => { bootLog = res.content || '(LOG_VACUUM)'; })
				.catch(() => { bootLog = '(LOG_FAILURE)'; })
				.finally(() => { bootLogLoading = false; });
		}
	});

	async function loadSnapshots() {
		if (!detail.summary.vm_id) return;
		snapshotsLoading = true;
		snapshotsError = null;
		try {
			const token = getStoredToken() ?? undefined;
			const res = await listVmSnapshots({ vm_id: detail.summary.vm_id }, token);
			snapshots = res.items;
		} catch (err: unknown) {
			snapshotsError = err instanceof Error ? err.message : 'Snapshot registry inaccessible';
		} finally {
			snapshotsLoading = false;
		}
	}

	$effect(() => {
		if (detail.currentTab === 'snapshots' && detail.summary.vm_id) {
			loadSnapshots();
		}
	});

	$effect(() => {
		if (detail.currentTab === 'events' && detail.summary.vm_id) {
			eventsLoading = true;
			eventsError = null;
			const token = getStoredToken() ?? undefined;
			listVmEvents(detail.summary.vm_id, token)
				.then(res => { events = res.items ?? []; })
				.catch(err => { eventsError = err instanceof Error ? err.message : 'Failed to load events'; })
				.finally(() => { eventsLoading = false; });
		}
	});

	async function retryDetailLoad() {
		await liveState.invalidateAndRefresh();
	}

	function normalizeTone(status: string): ShellTone {
		const s = status.toLowerCase();
		if (['running', 'healthy', 'ready', 'active', 'online'].includes(s)) return 'healthy';
		if (['warning', 'maintenance', 'starting', 'stopping', 'paused', 'rebooting'].includes(s)) return 'warning';
		if (['failed', 'error', 'critical', 'crashed', 'deleting'].includes(s)) return 'failed';
		return 'unknown';
	}

	async function executeAction(action: string) {
		pendingAction = action;
		const token = getStoredToken() ?? undefined;
		const vm_id = detail.summary.vm_id;

		try {
			if (action === 'delete') {
				await mutateWithRefresh(
					() => deleteVm({ vm_id, requested_by: 'webui' }, token),
					{
						patterns: ['vms:'],
						detailId: vm_id,
						delayMs: 2000,
						successMessage: `VM ${vm_id} delete accepted`,
						errorMessage: 'Mutation failed',
					}
				);
			} else {
				const apiAction = action === 'shutdown' ? 'stop' : action;
				const isForce = action === 'poweroff';
				await mutateWithRefresh(
					() => mutateVm({ vm_id, action: apiAction, force: isForce }, token),
					{
						patterns: ['vms:'],
						detailId: vm_id,
						delayMs: 2000,
						successMessage: `Workload ${action} accepted`,
						errorMessage: 'Mutation failed',
					}
				);
			}
		} catch (err: unknown) {
			// Error already toasted by mutateWithRefresh
		} finally {
			pendingAction = null;
		}
	}

	async function executeMigrate(targetNodeId: string, pauseFirst: boolean) {
		migrateSubmitting = true;
		const token = getStoredToken() ?? undefined;
		const vm_id = detail.summary.vm_id;

		try {
			await mutateWithRefresh(
				() =>
					mutateVm(
						{
							vm_id,
							action: 'migrate',
							force: false,
							target_node_id: targetNodeId,
							pause_first: pauseFirst
						},
						token
					),
				{
					patterns: ['vms:'],
					detailId: vm_id,
					successMessage: `Migration of VM ${vm_id} accepted`,
					errorMessage: 'Migration failed',
				}
			);
			migrateModalOpen = false;
		} catch (err: any) {
			// Error already toasted by mutateWithRefresh
		} finally {
			migrateSubmitting = false;
		}
	}

	const configProps = $derived(detail.configuration.map(c => ({ label: c.label, value: c.value })));
</script>

<div class="inventory-page">
	{#if detail.state === 'error'}
		<VmDetailErrorState
			vmId={detail.summary.vm_id}
			requestedVmId={data.requestedVmId}
			currentTab={detail.currentTab}
			nodeId={detail.summary.node_id}
			errorMessage={detail.errorMessage}
			onRetry={retryDetailLoad}
		/>
	{:else if detail.state === 'empty'}
		<EmptyInfrastructureState
			title="Workload Identity Unknown"
			description="The requested virtual entity is not recognized."
			hint="Return to the VM catalog and refresh the workload inventory."
		/>
	{:else}
		<VmDetailHeader
			title={detail.summary.name}
			eyebrow={`VM ID ${detail.summary.vm_id}`}
			statusLabel={detail.summary.power_state}
			tone={normalizeTone(detail.summary.power_state)}
			parentLabel="Virtual machines"
			parentHref="/vms"
			{pendingAction}
			powerState={detail.summary.power_state}
			onExecute={executeAction}
			onMigrate={() => { migrateModalOpen = true; }}
		/>

		<div class="tabs-area">
			<DetailTabs tabs={detail.sections} currentId={detail.currentTab} />
		</div>

		<main class="inventory-main" class:inventory-main--rail-open={supportRailOpen}>
			<section class="detail-content">
				<div style:display={detail.currentTab === 'console' ? 'block' : 'none'}>
					<VmConsoleTab
						vmId={detail.summary.vm_id}
						{consoleLoading}
						{liveConsoleUrl}
						{liveConsoleToken}
						{VmConsoleComponent}
						running={detail.summary.power_state.toLowerCase() === 'running'}
						consoleExpiresAt={liveConsoleExpiresAt}
						getConsoleUrl={async () => {
							const res = await getVmConsoleUrl(detail.summary.vm_id, getStoredToken() ?? undefined);
							liveConsoleUrl = res.url;
							liveConsoleToken = res.token;
							liveConsoleExpiresAt = res.expires_at;
							return { url: res.url, token: res.token };
						}}
					/>
				</div>
				{#if detail.currentTab === 'boot-log'}
					<VmBootLogTab {bootLogLoading} {bootLog} />
				{:else if detail.currentTab === 'snapshots'}
					<VmSnapshots
						vmId={detail.summary.vm_id}
						{snapshots}
						loading={snapshotsLoading}
						error={snapshotsError}
					/>
				{:else if detail.currentTab === 'events'}
					<VmTasksTab {eventsLoading} {eventsError} {events} />
				{:else if detail.currentTab === 'metrics'}
					<VmMetricsTab
						cpu={detail.summary.cpu}
						memory={detail.summary.memory}
						powerState={detail.summary.power_state}
						health={detail.summary.health}
						attachedVolumes={detail.summary.attached_volumes ?? []}
						attachedNics={detail.summary.attached_nics ?? []}
					/>
					<GuestAgentCard
						vmId={detail.summary.vm_id}
						vmName={detail.summary.name}
						onEnroll={() => (agentEnrollOpen = true)}
					/>
					<GuestFilesystemsCard vmId={detail.summary.vm_id} />
					<GuestChecksCard vmId={detail.summary.vm_id} />
					<GuestProcessesCard vmId={detail.summary.vm_id} />
					<TargetMonitoringPanel
						targetKind="vm"
						targetId={detail.summary.vm_id}
						metricIds={[
							'vm.cpu.cores_used',
							'vm.memory.host_accounted_bytes',
							'vm.memory.guest_available_bytes',
							'vm.block.read_bytes_total',
							'vm.block.write_bytes_total',
							'vm.net.rx_bytes_total',
							'vm.net.tx_bytes_total',
							'vm.guest.cpu.utilization_ratio',
							'vm.guest.load1',
							'vm.guest.uptime_seconds'
						]}
					/>
				{:else if detail.currentTab === 'summary'}
					<VmDetailSummaryTab
						powerState={detail.summary.power_state}
						health={detail.summary.health}
						cpu={detail.summary.cpu}
						memory={detail.summary.memory}
						volumes={detail.summary.attached_volumes ?? []}
						nics={detail.summary.attached_nics ?? []}
						recentTasks={detail.recent_tasks}
					/>
				{/if}
			</section>

			<VmDetailSupportRail
				nodeId={detail.summary.node_id}
				{configProps}
				open={supportRailOpen}
				onToggle={() => supportRailOpen = !supportRailOpen}
			/>
		</main>
	{/if}
</div>

<GuestAgentEnrollDialog
	open={agentEnrollOpen}
	vmId={detail?.summary?.vm_id ?? ''}
	vmName={detail?.summary?.name}
	onClose={() => (agentEnrollOpen = false)}
/>

<VmMigrateModal
	bind:open={migrateModalOpen}
	vmId={detail.summary.vm_id}
	currentNodeId={detail.summary.node_id}
	submitting={migrateSubmitting}
	onmigrate={executeMigrate}
	onclose={() => { migrateModalOpen = false; }}
/>

<style>
	.inventory-page {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	.tabs-area {
		margin-top: -0.25rem;
	}

	.inventory-main {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 2.4rem;
		gap: 1rem;
		align-items: start;
	}

	.inventory-main--rail-open {
		grid-template-columns: minmax(0, 1.65fr) minmax(17rem, 0.9fr);
	}

	.detail-content {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		min-width: 0;
	}

	@media (max-width: 1200px) {
		.inventory-main {
			grid-template-columns: 1fr;
		}
	}

	@media (max-width: 720px) {
		.tabs-area {
			margin-top: 0;
		}
	}
</style>
