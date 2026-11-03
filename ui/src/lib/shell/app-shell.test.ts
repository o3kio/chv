import { describe, expect, it } from 'vitest';

import { getPageDefinition, getTopLevelPageDefinitions } from '#lib/shell/app-shell.ts';

describe('app shell definitions', () => {
	it('matches the accepted top-level IA order', () => {
		expect(getTopLevelPageDefinitions().map((page) => page.href)).toEqual([
			'/',
			'/clusters',
			'/nodes',
			'/vms',
			'/volumes',
			'/networks',
			'/images',
			'/tasks',
			'/events',
			'/alerts',
			'/maintenance',
			'/settings'
		]);
	});

	it('can resolve current-page metadata from a concrete route', () => {
		expect(getPageDefinition('/vms').title).toBe('Instances');
		expect(getPageDefinition('/events').navLabel).toBe('Events');
		expect(getPageDefinition('/unknown').title).toBe('Overview');
	});

	it('maps legacy top-level routes to the closest shell section instead of Overview', () => {
		expect(getPageDefinition('/storage').title).toBe('Storage Pools');
		expect(getPageDefinition('/operations').title).toBe('Tasks');
		expect(getPageDefinition('/templates').title).toBe('Images');
		expect(getPageDefinition('/maintenance').title).toBe('Maintenance');
		expect(getPageDefinition('/quotas').title).toBe('Settings');
		expect(getPageDefinition('/observability').title).toBe('Overview');
	});
});
