import type { ShellTone } from '#lib/shell/app-shell.ts';

/**
 * Ready states reported by the control plane (HostReady, StorageReady,
 * NetworkReady, TenantReady). Matching stays case-insensitive so the UI
 * accepts both the domain vocabulary and legacy lowercase values.
 */
export function isNodeReady(state: string): boolean {
	const normalized = state.trim().toLowerCase();
	return normalized.endsWith('ready') || normalized === 'online';
}

/** Map an observed node state plus health signal to a display tone. */
export function nodeStateTone(state: string, health?: string): ShellTone {
	const normalized = state.trim().toLowerCase();
	const healthTone = (health ?? '').trim().toLowerCase();

	if (
		normalized === 'failed' ||
		normalized === 'unreachable' ||
		normalized === 'error'
	) {
		return 'failed';
	}
	if (healthTone === 'critical' || healthTone === 'failed') return 'failed';

	if (normalized === 'degraded' || healthTone === 'warning' || healthTone === 'degraded') {
		return 'degraded';
	}

	if (
		normalized === 'maintenance' ||
		normalized === 'draining' ||
		normalized === 'discovered' ||
		normalized === 'bootstrapping'
	) {
		return 'warning';
	}

	if (isNodeReady(normalized)) return 'healthy';

	return 'unknown';
}

/**
 * Match a node against a state filter. Exact (case-insensitive) state names
 * win; the legacy online/offline/error aliases still work for old bookmarks.
 */
export function matchesNodeStateFilter(state: string, filter: string): boolean {
	const normalized = state.trim().toLowerCase();
	const wanted = filter.trim().toLowerCase();

	if (normalized === wanted) return true;
	if (wanted === 'online' || wanted === 'ready') return isNodeReady(normalized);
	if (wanted === 'offline') return normalized === 'unreachable';
	if (wanted === 'error') return normalized === 'failed';
	return false;
}
