import { describe, expect, it } from 'vitest';

import { isNodeReady, matchesNodeStateFilter, nodeStateTone } from './node-state';

describe('node state vocabulary', () => {
	it('treats ready states as operational, case-insensitively', () => {
		expect(isNodeReady('HostReady')).toBe(true);
		expect(isNodeReady('storageready')).toBe(true);
		expect(isNodeReady('TenantReady')).toBe(true);
		expect(isNodeReady('Degraded')).toBe(false);
		expect(isNodeReady('Draining')).toBe(false);
	});

	it('maps observed states to display tones', () => {
		expect(nodeStateTone('TenantReady', 'healthy')).toBe('healthy');
		expect(nodeStateTone('HostReady', 'unknown')).toBe('healthy');
		expect(nodeStateTone('Degraded', 'warning')).toBe('degraded');
		expect(nodeStateTone('Draining', 'healthy')).toBe('warning');
		expect(nodeStateTone('Maintenance', 'healthy')).toBe('warning');
		expect(nodeStateTone('Failed', 'critical')).toBe('failed');
		expect(nodeStateTone('Unreachable', 'unknown')).toBe('failed');
	});

	it('matches exact states and legacy filter aliases', () => {
		expect(matchesNodeStateFilter('TenantReady', 'tenantready')).toBe(true);
		expect(matchesNodeStateFilter('HostReady', 'Degraded')).toBe(false);
		expect(matchesNodeStateFilter('StorageReady', 'online')).toBe(true);
		expect(matchesNodeStateFilter('Unreachable', 'offline')).toBe(true);
		expect(matchesNodeStateFilter('Failed', 'error')).toBe(true);
	});
});
