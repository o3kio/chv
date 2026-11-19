<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import {
		ChevronDown,
		Loader2,
		Server,
		Box,
		MoreVertical
	} from 'lucide-svelte';
	import { liveState } from '#lib/stores/live-state.svelte.ts';
	import { selection } from '#lib/stores/selection.svelte.ts';
	import InstanceStatusBadge from './InstanceStatusBadge.svelte';
	import type { InstanceTreeItem } from '#lib/api/types.ts';

	interface Props {
		openGroups: Record<string, boolean>;
		searchQuery: string;
		onToggleGroup: (label: string) => void;
		onSelectVm: (vmId: string, vmName: string) => void;
		onContextMenu: (event: MouseEvent, instance: InstanceTreeItem) => void;
		onKebabClick: (event: MouseEvent, instance: InstanceTreeItem) => void;
	}

	let {
		openGroups,
		searchQuery,
		onToggleGroup,
		onSelectVm,
		onContextMenu,
		onKebabClick
	}: Props = $props();

	function isActive(href: string, pathname: string): boolean {
		if (href === '/') return pathname === '/';
		return pathname === href || pathname.startsWith(`${href}/`);
	}

	function getVmNodeId(vm: (typeof liveState.vms)[number]): string {
		return vm.node_id ?? 'unassigned';
	}

	const filteredNodes = $derived(
		searchQuery.trim() === ''
			? liveState.nodes
			: liveState.nodes.filter((n) =>
					n.name.toLowerCase().includes(searchQuery.toLowerCase())
				)
	);

	const filteredVms = $derived(
		searchQuery.trim() === ''
			? liveState.vms
			: liveState.vms.filter((v) =>
					v.name.toLowerCase().includes(searchQuery.toLowerCase())
				)
	);

	const vmsByNode = $derived(
		(() => {
			const map = new Map<string, typeof liveState.vms>();
			for (const vm of filteredVms) {
				const nodeId = getVmNodeId(vm);
				const list = map.get(nodeId) ?? [];
				list.push(vm);
				map.set(nodeId, list);
			}
			return map;
		})()
	);

	function getNodeExpandedKey(nodeId: string): string {
		return `host-${nodeId}`;
	}

	function vmToTreeItem(vm: (typeof liveState.vms)[number]): InstanceTreeItem {
		return {
			id: vm.id,
			name: vm.name,
			nodeId: getVmNodeId(vm),
			status: (vm.actual_state === 'running' ? 'running' : vm.actual_state === 'failed' ? 'failed' : 'stopped') as InstanceTreeItem['status']
		};
	}
</script>

<div class="flex flex-col gap-1">
	<div class="text-[10px] font-bold uppercase text-neutral-500 mb-1 pl-2 tracking-wider">Infrastructure</div>

	<div class="flex flex-col pl-2">
		{#if liveState.inventoryLoading}
			<div class="py-2 px-4 text-[10px] text-neutral-500 flex items-center gap-2">
				<Loader2 size={12} class="animate-spin" />
				<span>Syncing fleet...</span>
			</div>
		{:else if filteredNodes.length === 0}
			<div class="py-2 px-4 text-[10px] text-neutral-500">No hosts enrolled.</div>
		{:else}
			<div class="flex flex-col">
				<button
					type="button"
					class="flex items-center gap-2 text-(length:--chv-text-sm) text-neutral-400 no-underline bg-transparent border-none cursor-pointer rounded-xs text-left py-1 pr-2 pl-0 hover:bg-neutral-800 hover:text-(--color-sidebar-text-active,#ffffff)"
					aria-expanded={openGroups['cloud-1']}
					aria-controls="group-cloud-1"
					onclick={() => onToggleGroup('cloud-1')}
				>
					<ChevronDown size={10} class={!openGroups['cloud-1'] ? '-rotate-90' : ''} />
					<Server size={12} />
					<span>Default Cloud</span>
				</button>

				{#if openGroups['cloud-1']}
					<div id="group-cloud-1" class="ml-2 pl-2 border-l border-neutral-700 flex flex-col gap-0.5">
						<div class="flex flex-col">
							<button type="button"
								class="flex items-center gap-2 text-(length:--chv-text-sm) text-neutral-400 no-underline bg-transparent border-none cursor-pointer rounded-xs text-left py-1 pr-2 pl-0 hover:bg-neutral-800 hover:text-(--color-sidebar-text-active,#ffffff)"
								aria-expanded={openGroups['hosts']}
								aria-controls="group-hosts"
								onclick={() => onToggleGroup('hosts')}
							>
								<ChevronDown size={10} class={!openGroups['hosts'] ? '-rotate-90' : ''} />
								<Server size={12} />
								<span>Hosts</span>
							</button>

							{#if openGroups['hosts']}
								<div id="group-hosts" class="ml-2 pl-2 border-l border-neutral-700 flex flex-col gap-0.5">
									{#each filteredNodes as node}
										{@const hostExpanded = openGroups[getNodeExpandedKey(node.id)] ?? true}
										{@const hostVms = vmsByNode.get(node.id) ?? []}
										<div class="flex flex-col">
												<button
														type="button"
														class="app-nav__tree-row app-nav__tree-row--host"
														aria-expanded={hostExpanded}
														aria-controls="group-{node.id}"
														onclick={() => onToggleGroup(getNodeExpandedKey(node.id))}
													>
														<ChevronDown size={10} class={!hostExpanded ? '-rotate-90' : ''} />
														<div
															class="w-1.5 h-1.5 rounded-full {node.status === 'online' ? 'bg-success' : node.status === 'error' ? 'bg-danger' : 'bg-neutral-600'}"
															aria-hidden="true"
														></div>
														<span class="truncate">{node.name}</span>
													</button>

													{#if hostExpanded}
														<div id="group-{node.id}" class="ml-2 pl-2 border-l border-neutral-700 flex flex-col gap-0.5">
															<div class="app-nav__instance-list app-nav__instance-list--direct">
																{#if hostVms.length === 0}
																	<div class="py-1 px-2 text-[10px] text-neutral-500">No instances.</div>
																{:else}
																	{#each hostVms as vm}
																		{@const inst = vmToTreeItem(vm)}
																		{@const isVmActive = isActive(`/vms/${vm.id}`, page.url.pathname)}
																		<div
																			class="app-nav__instance-row group
																			{isVmActive ? 'app-nav__tree-link--active' : 'hover:bg-neutral-800 hover:text-(--color-sidebar-text-active,#ffffff) text-neutral-400'}"
																			role="button"
																			tabindex="0"
																			onclick={() => onSelectVm(vm.id, vm.name)}
																			onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelectVm(vm.id, vm.name); } }}
																			oncontextmenu={(e) => onContextMenu(e, inst)}
																		>
																			<InstanceStatusBadge status={inst.status} showText={false} />
																			<span class="truncate flex-1 min-w-0">{vm.name}</span>
																			<button
																					type="button"
																					class="app-nav__instance-action"
																					aria-label="Actions for instance {vm.name}"
																					onclick={(e) => onKebabClick(e, inst)}
																				>
																					<MoreVertical size={12} />
																				</button>
																			</div>
																	{/each}
																{/if}
															</div>
														</div>
													{/if}
										</div>
									{/each}
								</div>
							{/if}
						</div>
					</div>
				{/if}
			</div>

			<a
				href="/vms"
				class="app-nav__infrastructure-link {isActive('/vms', page.url.pathname) ? 'app-nav__tree-link--active' : ''}"
				aria-current={isActive('/vms', page.url.pathname) ? 'page' : undefined}
			>
				<span class="app-nav__tree-spacer" aria-hidden="true"></span>
				<Box size={12} />
				<span class="truncate">Instances</span>
				{#if filteredVms.length > 0}
					<span class="app-nav__tree-count">{filteredVms.length}</span>
				{/if}
			</a>
		{/if}
	</div>
</div>

<style>
	.app-nav__tree-row {
		display: grid;
		grid-template-columns: 0.75rem 0.875rem minmax(0, 1fr) auto;
		align-items: center;
		gap: 0.35rem;
		min-height: 1.625rem;
		padding: 0.25rem 0.45rem;
		border: 0;
		border-radius: var(--radius-xs);
		background: transparent;
		color: var(--chv-color-neutral-400);
		cursor: pointer;
		font-size: var(--chv-text-xs);
		text-align: left;
		text-decoration: none;
		transition:
			background-color 120ms ease-in-out,
			color 120ms ease-in-out;
	}

	.app-nav__tree-row--host {
		font-size: var(--chv-text-sm);
	}

	.app-nav__tree-row:hover {
		background: var(--chv-color-neutral-800);
		color: var(--color-sidebar-text-active, #ffffff);
	}

	.app-nav__infrastructure-link {
		display: grid;
		grid-template-columns: 0.75rem 0.875rem minmax(0, 1fr) auto;
		align-items: center;
		gap: 0.35rem;
		min-height: 1.625rem;
		padding: 0.25rem 0.45rem;
		border-radius: var(--radius-xs);
		color: var(--chv-color-neutral-400);
		font-size: var(--chv-text-sm);
		text-decoration: none;
		transition:
			background-color 120ms ease-in-out,
			color 120ms ease-in-out;
	}

	.app-nav__infrastructure-link:hover {
		background: var(--chv-color-neutral-800);
		color: var(--color-sidebar-text-active, #ffffff);
	}

	.app-nav__tree-spacer {
		width: 0.75rem;
	}

	.app-nav__tree-count {
		font-size: 10px;
		color: var(--chv-color-neutral-500);
	}

	.app-nav__instance-list {
		display: flex;
		flex-direction: column;
		gap: 0.125rem;
		margin-left: 1.225rem;
		padding-left: 0.45rem;
		border-left: 1px solid var(--chv-color-neutral-700);
	}

	.app-nav__instance-list--direct {
		margin-left: 0;
	}

	.app-nav__instance-row {
		position: relative;
		display: grid;
		grid-template-columns: 0.875rem minmax(0, 1fr) 1.25rem;
		align-items: center;
		gap: 0.35rem;
		min-height: 1.625rem;
		padding: 0.25rem 0.35rem;
		border-radius: var(--radius-xs);
		font-size: var(--chv-text-xs);
		transition:
			background-color 120ms ease-in-out,
			color 120ms ease-in-out;
	}

	.app-nav__instance-action {
		display: grid;
		place-items: center;
		width: 1.25rem;
		height: 1.25rem;
		padding: 0;
		border: 0;
		border-radius: var(--radius-xs);
		background: transparent;
		color: var(--chv-color-neutral-400);
		cursor: pointer;
		opacity: 0;
		transition:
			opacity 120ms ease-in-out,
			background-color 120ms ease-in-out,
			color 120ms ease-in-out;
	}

	.group:hover .app-nav__instance-action,
	.group:focus-within .app-nav__instance-action {
		opacity: 1;
	}

	.app-nav__instance-action:hover {
		background: var(--chv-color-neutral-700);
		color: var(--color-sidebar-text-active, #ffffff);
	}

	.app-nav__tree-link--active {
		background: rgba(var(--color-primary-rgb), 0.15) !important;
		color: var(--color-primary) !important;
		border-left: 2px solid var(--color-primary);
		padding-left: calc(0.5rem - 2px);
	}
</style>
