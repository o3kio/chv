import { browser } from '$app/env';
import { refreshAll } from '$app/navigation';
import { isLoggedIn } from '#lib/api/client.ts';
import type { NodeWithResources, VM } from '#lib/api/types.ts';
import { listNodes } from '#lib/bff/nodes.ts';
import { listVms } from '#lib/bff/vms.ts';
import type { NodeListItem, VmListItem } from '#lib/bff/types.ts';
import { taskStream, type TaskUpdate } from './task-stream.svelte';

export interface InvalidateOpts {
	patterns?: string[];
	sidebar?: boolean;
	detailId?: string;
	delayMs?: number;
}

class LiveState {
	private readonly taskPatternMap: Record<string, string> = {
		CreateVm: 'vms:',
		StartVm: 'vms:',
		ShutdownVm: 'vms:',
		PoweroffVm: 'vms:',
		RestartVm: 'vms:',
		DeleteVm: 'vms:',
		MigrateVm: 'vms:',
		SnapshotVm: 'vms:',
		RestoreSnapshot: 'vms:',
		CreateNode: 'nodes:',
		DeleteNode: 'nodes:',
		CreateNetwork: 'networks:',
		DeleteNetwork: 'networks:',
		CreateVolume: 'volumes:',
		DeleteVolume: 'volumes:',
		ResizeVolume: 'volumes:',
		ImportImage: 'images:',
		DeleteImage: 'images:',
		CreateVmTemplate: 'templates:',
		DeleteVmTemplate: 'templates:',
	};

	private handleTaskCompleted(task: TaskUpdate) {
		const pattern = this.taskPatternMap[task.summary];
		if (!pattern) return;
		this.invalidateAndRefresh({
			patterns: [pattern],
			sidebar: true,
			detailId: task.resource_id,
		}).catch((err) => {
			// TODO: integrate structured logger instead of console
			// eslint-disable-next-line no-console
			console.error('[liveState] invalidateAndRefresh failed for completed task:', err);
		});
	}

	constructor() {
		if (browser) {
			taskStream.onTaskCompleted = (task) => this.handleTaskCompleted(task);
		}
	}

	// Inventory state
	nodes = $state<NodeWithResources[]>([]);
	vms = $state<VM[]>([]);
	inventoryLoading = $state(true);

	// Derived
	pinnedVms = $derived(
		this.vms
			.filter((v) => v.actual_state === 'running')
			.slice(0, 3)
	);

	private normalizeNodeStatus(state: string): NodeWithResources['status'] {
		const s = state.toLowerCase();
		if (s.includes('ready') || s.includes('online') || s.includes('active')) return 'online';
		if (s.includes('error') || s.includes('fail')) return 'error';
		if (s.includes('maint')) return 'maintenance';
		return 'offline';
	}

	private normalizeVmState(state: string): string {
		return state.toLowerCase();
	}

	private mapNode(item: NodeListItem): NodeWithResources {
		return {
			id: item.node_id,
			name: item.name,
			hostname: item.name,
			ip_address: '',
			status: this.normalizeNodeStatus(item.state),
			is_local: false,
			resources: { vms: 0, images: 0, storage_pools: 0, networks: 0 },
			capabilities: '',
			last_seen_at: '',
			created_at: '',
			updated_at: '',
		};
	}

	private mapVm(item: VmListItem): VM {
		const state = this.normalizeVmState(item.power_state);
		return {
			id: item.vm_id,
			name: item.name,
			node_id: item.node_id,
			image_id: '',
			storage_pool_id: '',
			network_id: '',
			desired_state: state,
			actual_state: state,
			vcpu: 0,
			memory_mb: 0,
			disk_path: '',
			seed_iso_path: '',
			workspace_path: '',
			ip_address: '',
			mac_address: '',
			console_type: 'serial',
		};
	}

	async fetchInventory() {
		if (!isLoggedIn()) {
			this.inventoryLoading = false;
			return;
		}
		try {
			const [nodesRes, vmsRes] = await Promise.all([
				listNodes({ page: 1, page_size: 100, filters: {} }),
				listVms({ page: 1, page_size: 100, filters: {} }),
			]);
			this.nodes = (nodesRes.items || []).map((item) => this.mapNode(item));
			this.vms = (vmsRes.items || []).map((item) => this.mapVm(item));
		} catch (err) {
			// TODO: integrate structured logger instead of console
			// eslint-disable-next-line no-console
			console.error('Failed to load inventory:', err);
			this.nodes = [];
			this.vms = [];
		} finally {
			this.inventoryLoading = false;
		}
	}

	// Cache
	private cache = new Map<string, { data: unknown; timestamp: number; ttl: number }>();
	private readonly LIST_TTL = 30_000;

	private isCacheFresh(entry: { timestamp: number; ttl: number }) {
		return Date.now() - entry.timestamp < entry.ttl;
	}

	async cachedFetch<T>(key: string, fetcher: () => Promise<T>, ttlMs?: number): Promise<T> {
		if (!browser) return fetcher();
		const entry = this.cache.get(key) as { data: T; timestamp: number; ttl: number } | undefined;
		if (entry && this.isCacheFresh(entry)) return entry.data;

		try {
			const data = await fetcher();
			this.cache.set(key, { data, timestamp: Date.now(), ttl: ttlMs ?? this.LIST_TTL });
			return data;
		} catch (err) {
			if (entry) {
				// TODO: integrate structured logger instead of console
				// eslint-disable-next-line no-console
				console.warn(`[liveState] fetch error for key "${key}", returning stale data`, err);
				return entry.data;
			}
			throw err;
		}
	}

	invalidateCache(key: string) {
		if (!browser) return;
		this.cache.delete(key);
	}

	invalidateCachePattern(prefix: string) {
		if (!browser) return;
		for (const k of this.cache.keys()) {
			if (k.startsWith(prefix)) this.cache.delete(k);
		}
	}

	clearCache() {
		if (!browser) return;
		this.cache.clear();
	}

	// Polling
	private inventoryPollId: ReturnType<typeof setInterval> | null = null;

	startInventoryPolling(intervalMs = 10_000) {
		this.stopInventoryPolling();
		this.inventoryPollId = setInterval(() => {
			this.fetchInventory();
		}, intervalMs);
	}

	stopInventoryPolling() {
		if (this.inventoryPollId) {
			clearInterval(this.inventoryPollId);
			this.inventoryPollId = null;
		}
	}

	async invalidateAndRefresh(opts: InvalidateOpts = {}) {
		if (!browser) return;

		if (opts.patterns) {
			for (const p of opts.patterns) {
				this.invalidateCachePattern(p);
				// When a specific resource id is provided, also drop the
				// per-resource cache key (e.g. "vms:abc-123") so the detail
				// view picks up the post-mutation state on next read.
				if (opts.detailId) {
					this.invalidateCache(`${p}${opts.detailId}`);
				}
			}
		}

		if (opts.sidebar) {
			await this.fetchInventory();
		}

		await refreshAll();

		if (opts.delayMs && opts.delayMs > 0) {
			setTimeout(() => {
				const tasks: Promise<unknown>[] = [];
				if (opts.patterns) {
					for (const p of opts.patterns) {
						this.invalidateCachePattern(p);
						if (opts.detailId) {
							this.invalidateCache(`${p}${opts.detailId}`);
						}
					}
				}
				if (opts.sidebar) {
					tasks.push(this.fetchInventory());
				}
				tasks.push(refreshAll());
				Promise.all(tasks).catch((err) => {
					// TODO: integrate structured logger instead of console
					// eslint-disable-next-line no-console
					console.warn('[liveState] deferred invalidateAndRefresh failed:', err);
				});
			}, opts.delayMs);
		}
	}
}

export const liveState = new LiveState();
