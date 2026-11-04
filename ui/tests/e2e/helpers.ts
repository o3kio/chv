import type { Page } from '@playwright/test';

/**
 * Seed the non-credential UX hint cookies the SPA reads (#660). The browser
 * session itself is the HttpOnly `chv_session` cookie, which these tests
 * cannot forge; API calls are mocked so no real credential is needed.
 */
export async function loginAsAdmin(page: Page) {
	await page.context().addCookies([
		{ name: 'chv_logged_in', value: '1', domain: 'localhost', path: '/', sameSite: 'Strict' },
		{ name: 'chv_role', value: 'admin', domain: 'localhost', path: '/', sameSite: 'Strict' }
	]);
}

/**
 * Wait until SvelteKit has hydrated the current page. The root layout's
 * `onMount` sets `data-theme` on <html>; that hook cannot run until the
 * component tree is interactive, so it is a reliable readiness signal for
 * tests that click immediately after `goto`. Without it, a click can land on
 * the server-rendered button before its handler is attached and be dropped.
 */
export async function waitForHydration(page: Page) {
	await page.waitForFunction(() => document.documentElement.dataset.theme !== undefined);
}

export async function mockApiResponse(
	page: Page,
	urlPattern: string | RegExp,
	json: unknown,
	status = 200,
	headers: Record<string, string> = {}
) {
	await page.route(urlPattern, async (route) => {
		await route.fulfill({
			status,
			contentType: 'application/json',
			headers,
			body: JSON.stringify(json)
		});
	});
}

export async function setupCommonMocks(page: Page) {
	await mockApiResponse(page, '/v1/overview', mockOverview);
	await mockApiResponse(page, '/v1/nodes', mockNodes);
	await mockApiResponse(page, '/v1/vms', mockVms);
}

export async function navigateClientSide(page: Page, url: string) {
	await page.evaluate((targetUrl) => {
		const a = document.createElement('a');
		a.href = targetUrl;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}, url);
	await page.waitForURL(url);
}

export const mockOverview = {
	clusters_total: 1,
	clusters_healthy: 1,
	clusters_degraded: 0,
	nodes_total: 1,
	nodes_degraded: 0,
	vms_running: 1,
	vms_total: 2,
	active_tasks: 0,
	unresolved_alerts: 0,
	maintenance_nodes: 0,
	capacity_hotspots: 0,
	cpu_usage_percent: 25,
	memory_usage_percent: 40,
	storage_usage_percent: 15,
	alerts: [],
	recent_tasks: []
};

export const mockVms = {
	items: [
		{
			vm_id: 'vm-1',
			name: 'web-server',
			node_id: 'node-1',
			power_state: 'running',
			health: 'healthy',
			cpu: '2',
			memory: '4 GB',
			volume_count: 1,
			nic_count: 1,
			last_task: 'created',
			alerts: 0
		},
		{
			vm_id: 'vm-2',
			name: 'db-server',
			node_id: 'node-1',
			power_state: 'stopped',
			health: 'healthy',
			cpu: '4',
			memory: '8 GB',
			volume_count: 2,
			nic_count: 1,
			last_task: 'stopped',
			alerts: 0
		}
	],
	page: { page: 1, page_size: 50, total_items: 2 },
	filters: { applied: {} }
};

export const mockNodes = {
	items: [
		{
			node_id: 'node-1',
			name: 'hv-01',
			cluster: 'default',
			state: 'online',
			health: 'healthy',
			cpu: '16',
			memory: '64 GB',
			storage: '1 TB',
			network: '10 Gbps',
			version: 'v1.0.0',
			maintenance: false,
			active_tasks: 0,
			alerts: 0,
			hypervisor_capabilities: ['kvm']
		}
	],
	page: { page: 1, page_size: 50, total_items: 1 },
	filters: { applied: {} }
};

export const mockNodeDetail = {
	state: 'ready',
	summary: {
		node_id: 'node-1',
		name: 'hv-01',
		cluster: 'default',
		state: 'online',
		health: 'healthy',
		version: 'v1.0.0',
		cpu: '16',
		memory: '64 GB',
		storage: '1 TB',
		network: '10 Gbps',
		maintenance: false,
		scheduling: true,
		uptime: '42d',
		last_checkin: '2024-01-01T00:00:00Z',
		hypervisor_capabilities: ['kvm']
	},
	sections: [
		{ id: 'summary', label: 'Summary' },
		{ id: 'vms', label: 'VMs', count: 2 },
		{ id: 'tasks', label: 'Tasks', count: 0 },
		{ id: 'configuration', label: 'Configuration' }
	],
	hostedVms: [
		{ vm_id: 'vm-1', name: 'web-server', power_state: 'running', health: 'healthy', cpu: '2', memory: '4 GB' },
		{ vm_id: 'vm-2', name: 'db-server', power_state: 'stopped', health: 'healthy', cpu: '4', memory: '8 GB' }
	],
	recentTasks: [],
	configuration: [
		{ label: 'Node ID', value: 'node-1' },
		{ label: 'Version', value: 'v1.0.0' },
		{ label: 'CPU', value: '16' },
		{ label: 'Memory', value: '64 GB' },
		{ label: 'Storage backend', value: 'zfs' }
	]
};

export const mockNetworks = {
	items: [
		{
			network_id: 'net-1',
			name: 'default-net',
			scope: 'global',
			health: 'healthy',
			attached_vms: 2,
			exposure: 'private',
			policy: 'allow-all',
			last_task: 'created',
			alerts: 0,
			dhcp_enabled: true,
			ipam_mode: 'internal',
			is_default: true
		}
	],
	page: { page: 1, page_size: 50, total_items: 1 },
	filters: { applied: {} }
};

export const mockSettings = {
	version: 'v0.1.0',
	build: 'abc123',
	environment: 'test',
	api_endpoint: '/api/v1',
	session_ttl_hours: 24
};

export const mockHypervisorSettings = {
	settings: {
		cpu_nested: false,
		cpu_amx: false,
		cpu_kvm_hyperv: false,
		memory_mergeable: false,
		memory_hugepages: false,
		memory_shared: false,
		memory_prefault: false,
		iommu: false,
		rng_src: '/dev/urandom',
		watchdog: false,
		landlock_enable: false,
		serial_mode: 'Socket',
		console_mode: 'Pty',
		pvpanic: false,
		tpm_type: null,
		tpm_socket_path: null,
		profile_id: null
	},
	profiles: [
		{
			id: 'perf',
			name: 'Performance',
			description: 'High performance profile',
			cpu_nested: true,
			cpu_amx: true,
			cpu_kvm_hyperv: true,
			memory_mergeable: false,
			memory_hugepages: true,
			memory_shared: false,
			memory_prefault: false,
			iommu: false,
			rng_src: null,
			watchdog: null,
			landlock_enable: null,
			serial_mode: null,
			console_mode: null,
			pvpanic: null,
			tpm_type: null,
			tpm_socket_path: null,
			is_builtin: true
		}
	]
};

export const mockImages = {
	items: [
		{
			image_id: 'img-1',
			name: 'ubuntu-22.04',
			source_url: 'http://example.com/ubuntu.img',
			format: 'qcow2',
			status: 'ready'
		}
	],
	page: { page: 1, page_size: 50, total_items: 1 },
	filters: { applied: {} }
};
