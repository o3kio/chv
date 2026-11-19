<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import {
		House,
		Database,
		Search,
		Loader2,
		LayoutGrid,
		Compass,
		ChevronDown
	} from 'lucide-svelte';
	import { liveState } from '#lib/stores/live-state.svelte.ts';
	import { taskStream } from '#lib/stores/task-stream.svelte.ts';
	import { selection } from '#lib/stores/selection.svelte.ts';
	import { clearToken, getStoredToken } from '#lib/api/client.ts';
	import { mutateVm, deleteVm } from '#lib/bff/vms.ts';
	import { toast } from '#lib/stores/toast.svelte.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import { buildInstanceActions } from '#lib/shell/instance-actions.ts';
	import InstanceContextMenu from './InstanceContextMenu.svelte';
	import DeleteInstanceDialog from '#lib/components/vms/DeleteInstanceDialog.svelte';
	import PowerOffInstanceDialog from '#lib/components/vms/PowerOffInstanceDialog.svelte';
	import NavInfrastructureTree from './NavInfrastructureTree.svelte';
	import NavGlobalLinks from './NavGlobalLinks.svelte';
	import NavFooterControls from './NavFooterControls.svelte';
	import type { InstanceTreeItem } from '#lib/api/types.ts';

	function isActive(href: string, pathname: string): boolean {
		if (href === '/') return pathname === '/';
		return pathname === href || pathname.startsWith(`${href}/`);
	}

	let openGroups = $state<Record<string, boolean>>({
		'cloud-1': true,
		'hosts': true,
		'design': true
	});
	let searchQuery = $state('');
	let contextMenuInstance = $state<InstanceTreeItem | null>(null);
	let contextMenuPos = $state({ x: 0, y: 0 });
	let deleteDialogInstance = $state<InstanceTreeItem | null>(null);
	let poweroffDialogInstance = $state<InstanceTreeItem | null>(null);
	let pendingAction = $state<string | null>(null);

	let contextMenuRef = $state<InstanceContextMenu | null>(null);

	onMount(() => {
		liveState.fetchInventory();
		liveState.startInventoryPolling();
		taskStream.connect(['vm', 'node', 'network', 'volume', 'image']);

		return () => {
			liveState.stopInventoryPolling();
			taskStream.disconnect();
		};
	});

	function toggleGroup(label: string) {
		openGroups[label] = !openGroups[label];
	}

	function handleSelection(type: any, id: string, label: string) {
		selection.select(type, id, label);
	}

	async function handleLogout() {
		try {
			const { createAPIClient } = await import('#lib/api/client.ts');
			await createAPIClient().logout();
		} catch {
			// Best-effort
		} finally {
			clearToken();
			goto('/login');
		}
	}

	function handleInstanceContextMenu(event: MouseEvent, instance: InstanceTreeItem) {
		event.preventDefault();
		contextMenuInstance = instance;
		requestAnimationFrame(() => {
			contextMenuRef?.openAt(event.clientX, event.clientY);
		});
	}

	function handleKebabClick(event: MouseEvent, instance: InstanceTreeItem) {
		event.preventDefault();
		event.stopPropagation();
		const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
		contextMenuInstance = instance;
		requestAnimationFrame(() => {
			contextMenuRef?.openAt(rect.right - 8, rect.top + 8);
		});
	}

	function handleInstanceAction(actionId: string) {
		if (!contextMenuInstance) return;
		const inst = contextMenuInstance;

		switch (actionId) {
			case 'open':
				goto(`/vms/${inst.id}`);
				break;
			case 'console':
				goto(`/vms/${inst.id}?tab=console`);
				break;
			case 'start':
			case 'shutdown':
			case 'restart':
				executeLifecycleAction(inst, actionId);
				break;
			case 'poweroff':
				poweroffDialogInstance = inst;
				break;
			case 'delete':
				deleteDialogInstance = inst;
				break;
			case 'rename':
				toast.info('Rename is not yet supported');
				break;
		}
	}

	async function executeLifecycleAction(inst: InstanceTreeItem, action: string) {
		const token = getStoredToken() ?? undefined;
		pendingAction = action;
		try {
			const apiAction = action === 'shutdown' ? 'stop' : action;
			const isForce = false;
			await mutateWithRefresh(
				() => mutateVm({ vm_id: inst.id, action: apiAction, force: isForce }, token),
				{
					patterns: ['vms:'],
					successMessage: `${action} accepted for ${inst.name}`,
					errorMessage: `${action} failed`,
				}
			);
		} catch (err: any) {
			// Error already toasted by mutateWithRefresh
		} finally {
			pendingAction = null;
		}
	}

	async function handleDeleteConfirm() {
		if (!deleteDialogInstance) return;
		const inst = deleteDialogInstance;
		const token = getStoredToken() ?? undefined;
		pendingAction = 'delete';
		try {
			await mutateWithRefresh(
				() => deleteVm({ vm_id: inst.id, requested_by: 'webui' }, token),
				{
					patterns: ['vms:'],
					successMessage: `Instance ${inst.name} deleted`,
					errorMessage: 'Delete failed',
				}
			);
			deleteDialogInstance = null;
		} catch (err: any) {
			// Error already toasted by mutateWithRefresh
		} finally {
			pendingAction = null;
		}
	}

	async function handlePowerOffConfirm() {
		if (!poweroffDialogInstance) return;
		const inst = poweroffDialogInstance;
		const token = getStoredToken() ?? undefined;
		pendingAction = 'poweroff';
		try {
			await mutateWithRefresh(
				() => mutateVm({ vm_id: inst.id, action: 'stop', force: true }, token),
				{
					patterns: ['vms:'],
					successMessage: `Power off accepted for ${inst.name}`,
					errorMessage: 'Power off failed',
				}
			);
			poweroffDialogInstance = null;
		} catch (err: any) {
			// Error already toasted by mutateWithRefresh
		} finally {
			pendingAction = null;
		}
	}

	function handleSelectVm(vmId: string, vmName: string) {
		handleSelection('vm', vmId, vmName);
		goto(`/vms/${vmId}`);
	}
</script>

{#if contextMenuInstance}
	<InstanceContextMenu
		bind:this={contextMenuRef}
		actions={buildInstanceActions(contextMenuInstance.status)}
		onAction={handleInstanceAction}
		instanceName={contextMenuInstance.name}
	/>
{/if}

{#if deleteDialogInstance}
	<DeleteInstanceDialog
		bind:open={() => deleteDialogInstance !== null, (v) => { if (!v) deleteDialogInstance = null; }}
		instanceName={deleteDialogInstance.name}
		instanceId={deleteDialogInstance.id}
		onConfirm={handleDeleteConfirm}
		onCancel={() => { deleteDialogInstance = null; }}
	/>
{/if}

{#if poweroffDialogInstance}
	<PowerOffInstanceDialog
		bind:open={() => poweroffDialogInstance !== null, (v) => { if (!v) poweroffDialogInstance = null; }}
		instanceName={poweroffDialogInstance.name}
		onConfirm={handlePowerOffConfirm}
		onCancel={() => { poweroffDialogInstance = null; }}
	/>
{/if}

<nav class="flex flex-col h-full gap-4 select-none" aria-label="Primary">
	<!-- Header -->
	<div class="flex items-center gap-3 py-2 px-1">
		<div class="grid place-items-center w-8 h-8 rounded-sm bg-primary text-(--color-sidebar-text-active,#ffffff)">
			<Database size={16} />
		</div>
		<div class="flex flex-col">
			<div class="text-[0.875rem] font-bold text-(--color-sidebar-text-active,#ffffff)">CellHV</div>
			<div class="text-[0.625rem] text-neutral-500 uppercase tracking-wider">Control Plane</div>
		</div>
	</div>

	<!-- Search -->
	<div class="mx-1 flex min-h-8 items-center gap-2 rounded-xs border border-neutral-700 bg-neutral-800 px-2.5 text-neutral-400 transition-colors duration-120 ease-in-out focus-within:border-primary focus-within:text-(--color-sidebar-text-active,#ffffff)">
		<Search size={12} class="shrink-0" />
		<input
			type="search"
			placeholder="Search resources..."
			class="min-w-0 flex-1 border-0 bg-transparent py-[0.35rem] px-0 text-(length:--text-xs) text-(--color-sidebar-text-active,#ffffff) placeholder:text-neutral-500"
			bind:value={searchQuery}
			aria-label="Search fleet resources"
		/>
	</div>

	<!-- Scrollable content -->
	<div class="flex-1 flex flex-col gap-6 overflow-y-auto pr-2 app-nav__scrollbox">
		<!-- DESIGN group (Architecture Designer) — placed above Fleet Overview per ADR-001-Designer -->
		<div class="flex flex-col gap-1" data-testid="nav-design-group">
			<button
				type="button"
				class="flex items-center gap-2 text-[10px] font-bold uppercase tracking-wider text-neutral-500 bg-transparent border-none cursor-pointer rounded-xs px-2 py-1 hover:text-(--color-sidebar-text-active,#ffffff)"
				aria-expanded={openGroups['design']}
				aria-controls="group-design"
				onclick={() => toggleGroup('design')}
			>
				<ChevronDown size={10} class={!openGroups['design'] ? '-rotate-90' : ''} aria-hidden="true" />
				<span>Design</span>
			</button>

			{#if openGroups['design']}
				<div id="group-design" class="flex flex-col gap-1">
					<a
						href="/architectures/new"
						data-testid="nav-architecture-designer"
						class="flex items-center gap-2.5 py-[0.35rem] px-2 text-(length:--text-sm) text-neutral-300 no-underline rounded-xs transition-all duration-120 ease-in-out hover:bg-neutral-800 hover:text-(--color-sidebar-text-active,#ffffff) {isActive('/architectures/new', page.url.pathname) ? 'bg-primary text-(--color-sidebar-text-active,#ffffff)' : ''}"
						aria-current={isActive('/architectures/new', page.url.pathname) ? 'page' : undefined}
					>
						<Compass size={14} />
						<span>Architecture Designer</span>
					</a>
					<a
						href="/architectures"
						data-testid="nav-saved-topologies"
						class="flex items-center gap-2.5 py-[0.35rem] px-2 text-(length:--text-sm) text-neutral-300 no-underline rounded-xs transition-all duration-120 ease-in-out hover:bg-neutral-800 hover:text-(--color-sidebar-text-active,#ffffff) {page.url.pathname === '/architectures' || (page.url.pathname.startsWith('/architectures/') && !page.url.pathname.startsWith('/architectures/new')) ? 'bg-primary text-(--color-sidebar-text-active,#ffffff)' : ''}"
						aria-current={page.url.pathname === '/architectures' || (page.url.pathname.startsWith('/architectures/') && !page.url.pathname.startsWith('/architectures/new')) ? 'page' : undefined}
					>
						<LayoutGrid size={14} />
						<span>Saved Topologies</span>
					</a>
				</div>
			{/if}
		</div>

		<!-- Fleet Overview -->
		<div class="flex flex-col gap-1">
			<a
				href="/"
				class="flex items-center gap-2.5 py-[0.35rem] px-2 text-(length:--text-sm) text-neutral-300 no-underline rounded-xs transition-all duration-120 ease-in-out hover:bg-neutral-800 hover:text-(--color-sidebar-text-active,#ffffff) {isActive('/', page.url.pathname) ? 'bg-primary text-(--color-sidebar-text-active,#ffffff)' : ''}"
				aria-current={isActive('/', page.url.pathname) ? 'page' : undefined}
			>
				<House size={14} />
				<span>Fleet Overview</span>
			</a>
		</div>

		<NavInfrastructureTree
			{openGroups}
			{searchQuery}
			onToggleGroup={toggleGroup}
			onSelectVm={handleSelectVm}
			onContextMenu={handleInstanceContextMenu}
			onKebabClick={handleKebabClick}
		/>

		<NavGlobalLinks />
	</div>

	<!-- Footer controls -->
	<NavFooterControls onLogout={handleLogout} />

	<div class="stream-status" title={taskStream.status === 'open' ? 'Live updates connected' : taskStream.status === 'error' ? 'Reconnecting...' : 'Live updates off'}>
		<span class="status-dot" class:connected={taskStream.status === 'open'} class:error={taskStream.status === 'error'}></span>
		<span class="status-label">{taskStream.status === 'open' ? 'Live' : taskStream.status === 'error' ? 'Reconnecting' : 'Off'}</span>
	</div>
</nav>

<style>
	.app-nav__scrollbox::-webkit-scrollbar {
		width: 4px;
	}

	.stream-status {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		padding: 0.25rem 0.75rem;
		font-size: 0.7rem;
		color: var(--color-neutral-500);
	}
	.status-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--color-neutral-400);
	}
	.status-dot.connected {
		background: var(--color-success);
	}
	.status-dot.error {
		background: var(--color-danger);
	}
</style>
