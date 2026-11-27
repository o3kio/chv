import { describe, expect, it } from 'vitest';

import { buildMaintenanceView, formatMaintenanceTimestamp } from './maintenance';

describe('maintenance view model', () => {
	it('maps only the fields the BFF actually reports', () => {
		const view = buildMaintenanceView({
			windows: [
				{
					window_id: 'win-1',
					name: 'kernel upgrade',
					cluster: 'cluster-a',
					status: 'scheduled',
					start_time: '2026-11-27T10:00:00Z',
					end_time: '2026-11-27T12:00:00Z',
					affected_nodes: 0
				}
			],
			nodes: [
				{
					node_id: 'node-1',
					name: 'node-one',
					cluster: '',
					state: 'draining',
					task_id: 'task-1'
				}
			],
			pending_actions: 1
		});

		expect(view.windows).toHaveLength(1);
		expect(view.windows[0]).toMatchObject({
			window_id: 'win-1',
			name: 'kernel upgrade',
			status: 'scheduled'
		});
		expect(view.nodes[0]).toEqual({
			node_id: 'node-1',
			name: 'node-one',
			state: 'draining'
		});
		expect(view.pendingActions).toBe(1);
		expect(view.empty).toBe(false);
	});

	it('treats an all-empty response as empty', () => {
		const view = buildMaintenanceView({ windows: [], nodes: [], pending_actions: 0 });
		expect(view.empty).toBe(true);
	});

	it('never invents a timestamp for missing or invalid values', () => {
		expect(formatMaintenanceTimestamp(undefined)).toBe('—');
		expect(formatMaintenanceTimestamp(null)).toBe('—');
		expect(formatMaintenanceTimestamp('not-a-date')).toBe('not-a-date');
		expect(formatMaintenanceTimestamp('2026-11-27T10:00:00Z')).not.toBe('—');
	});
});
