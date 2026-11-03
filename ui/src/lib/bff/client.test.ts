import { beforeEach, describe, expect, it, vi } from 'vitest';
import { bffFetch, BFFError } from './client';
import { loadOverview } from './overview';
import { listNodes, getNode } from './nodes';
import { listVms, getVm, mutateVm } from './vms';
import { listTasks } from './tasks';

function jsonHeaders(): Headers {
	return new Headers({ 'content-type': 'application/json' });
}

describe('bffFetch', () => {
	beforeEach(() => {
		vi.restoreAllMocks();
		vi.stubGlobal('window', undefined);
		const g = globalThis as typeof globalThis & { process?: { env?: Record<string, string> } };
		if (g.process?.env) {
			delete g.process.env.BFF_BASE_URL;
			delete g.process.env.CHV_BFF_BASE_URL;
		}
	});

	it('returns parsed JSON on happy path', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ data: 'ok' })
		});
		vi.stubGlobal('fetch', fetchMock);

		const result = await bffFetch<{ data: string }>('/v1/test', { method: 'POST' });

		expect(result).toEqual({ data: 'ok' });
		expect(fetchMock).toHaveBeenCalledWith(
			'http://localhost:8080/v1/test',
			expect.objectContaining({
				method: 'POST',
				headers: expect.any(Headers)
			})
		);
	});

	it('injects Authorization header when token is provided', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ data: 'ok' })
		});
		vi.stubGlobal('fetch', fetchMock);

		await bffFetch('/v1/test', { method: 'POST', token: 'my-token' });

		const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		const headers = init.headers as Headers;
		expect(headers.get('Authorization')).toBe('Bearer my-token');
		expect(headers.get('Content-Type')).toBe('application/json');
	});

	it('sends cookies for browser sessions when no token is provided', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ data: 'ok' })
		});
		vi.stubGlobal('fetch', fetchMock);

		await bffFetch('/v1/test', { method: 'POST' });

		const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(init.credentials).toBe('include');
		expect((init.headers as Headers).get('Authorization')).toBeNull();
	});

	it('throws BFFError on 401 with server payload', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: false,
			status: 401,
			headers: jsonHeaders(),
			json: async () => ({ message: 'Unauthorized', code: 'UNAUTHORIZED' })
		});
		vi.stubGlobal('fetch', fetchMock);

		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toThrow(BFFError);
		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toMatchObject({
			status: 401,
			code: 'UNAUTHORIZED',
			message: 'Unauthorized'
		});
	});

	it('throws BFFError on 500 when body is not JSON', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: false,
			status: 500,
			headers: new Headers({ 'content-type': 'text/html' }),
			json: async () => {
				throw new Error('bad json');
			}
		});
		vi.stubGlobal('fetch', fetchMock);

		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toThrow(BFFError);
		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toMatchObject({
			status: 500,
			code: 'UNKNOWN_ERROR',
			message: 'Request failed with status 500'
		});
	});

	it('throws BFFError on 200 when body is HTML', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: new Headers({ 'content-type': 'text/html' }),
			text: async () => '<!doctype html><html><body>fallback</body></html>',
			json: async () => {
				throw new Error('bad json');
			}
		});
		vi.stubGlobal('fetch', fetchMock);

		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toMatchObject({
			status: 200,
			code: 'INVALID_RESPONSE'
		});
	});

	it('throws BFFError on network error', async () => {
		const fetchMock = vi.fn().mockRejectedValue(new TypeError('fetch failed'));
		vi.stubGlobal('fetch', fetchMock);

		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toThrow(BFFError);
		await expect(bffFetch('/v1/test', { method: 'POST' })).rejects.toMatchObject({
			status: 0,
			code: 'NETWORK_ERROR',
			message: 'fetch failed'
		});
	});

	it('uses BFF_BASE_URL env var', async () => {
		const g = globalThis as unknown as { process?: { env?: Record<string, string> } };
		if (!g.process) g.process = { env: {} };
		if (!g.process.env) g.process.env = {};
		g.process.env.BFF_BASE_URL = 'http://bff.example';
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ data: 'ok' })
		});
		vi.stubGlobal('fetch', fetchMock);

		await bffFetch('/v1/test', { method: 'POST' });
		expect(fetchMock).toHaveBeenCalledWith('http://bff.example/v1/test', expect.anything());
	});

	it('falls back to CHV_BFF_BASE_URL env var', async () => {
		const g = globalThis as unknown as { process?: { env?: Record<string, string> } };
		if (!g.process) g.process = { env: {} };
		if (!g.process.env) g.process.env = {};
		g.process.env.CHV_BFF_BASE_URL = 'http://chv.example';
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ data: 'ok' })
		});
		vi.stubGlobal('fetch', fetchMock);

		await bffFetch('/v1/test', { method: 'POST' });
		expect(fetchMock).toHaveBeenCalledWith('http://chv.example/v1/test', expect.anything());
	});
});

describe('overview', () => {
	it('loadOverview calls the correct endpoint', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({
				health_tiles: [],
				capacity_tiles: [],
				recent_tasks: [],
				active_alerts: []
			})
		});
		vi.stubGlobal('fetch', fetchMock);

		const result = await loadOverview('token-123');
		expect(result).toEqual({
			health_tiles: [],
			capacity_tiles: [],
			recent_tasks: [],
			active_alerts: []
		});

		const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(url).toContain('/v1/overview');
		expect(init.method).toBe('POST');
		const headers = init.headers as Headers;
		expect(headers.get('Authorization')).toBe('Bearer token-123');
	});
});

describe('nodes', () => {
	it('listNodes sends the request body', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ items: [], page: { page: 1, page_size: 10, total_items: 0 }, filters: { applied: {} } })
		});
		vi.stubGlobal('fetch', fetchMock);

		await listNodes({ page: 1, page_size: 10, filters: { cluster: 'a' } }, 'tok');

		const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(init.method).toBe('POST');
		expect(JSON.parse(init.body as string)).toEqual({ page: 1, page_size: 10, filters: { cluster: 'a' } });
	});

	it('getNode sends the node_id', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ summary: { node_id: 'n1', name: 'Node 1', cluster: '', state: '', health: '', version: '', cpu: '', memory: '', storage: '', network: '', recent_tasks: [] } })
		});
		vi.stubGlobal('fetch', fetchMock);

		await getNode({ node_id: 'n1' });

		const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(JSON.parse(init.body as string)).toEqual({ node_id: 'n1' });
	});
});

describe('vms', () => {
	it('listVms sends the request body', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ items: [], page: { page: 1, page_size: 10, total_items: 0 }, filters: { applied: {} } })
		});
		vi.stubGlobal('fetch', fetchMock);

		await listVms({ page: 1, page_size: 10, filters: {} });

		const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(init.method).toBe('POST');
	});

	it('mutateVm sends action and force', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ accepted: true, task_id: 't1', vm_id: 'v1', summary: 'started' })
		});
		vi.stubGlobal('fetch', fetchMock);

		await mutateVm({ vm_id: 'v1', action: 'start', force: false });

		const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(JSON.parse(init.body as string)).toEqual({ vm_id: 'v1', action: 'start', force: false });
	});
});

describe('tasks', () => {
	it('listTasks sends the request body', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ({ items: [], page: { page: 1, page_size: 10, total_items: 0 }, filters: { applied: {} } })
		});
		vi.stubGlobal('fetch', fetchMock);

		await listTasks({ page: 1, page_size: 10, filters: {} });

		const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(url).toContain('/v1/tasks');
		expect(init.method).toBe('POST');
	});
});
