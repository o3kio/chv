import { beforeEach, describe, expect, it, vi } from 'vitest';

// task-stream now imports isLoggedIn from #lib/api/client, which transitively
// pulls in SvelteKit's $app/env/public and $app/navigation. Mock both so this
// suite can run under jsdom without a SvelteKit runtime.
vi.mock('$app/env/public', () => ({
	PUBLIC_CHV_API_BASE_URL: ''
}));

vi.mock('$app/navigation', () => ({
	goto: vi.fn(),
	refreshAll: vi.fn()
}));

import { TaskStreamStore, type TaskUpdate } from './task-stream.svelte';

function makeMockReader(chunks: Uint8Array[], hang = false) {
	let idx = 0;
	return {
		read: vi.fn(() => {
			if (hang) {
				return new Promise(() => {}); // Never resolves
			}
			if (idx < chunks.length) {
				return Promise.resolve({ done: false, value: chunks[idx++] });
			}
			return Promise.resolve({ done: true });
		}),
		releaseLock: vi.fn(),
	};
}

function mockFetchWithStream(chunks: Uint8Array[], status = 200, hang = false) {
	const reader = makeMockReader(chunks, hang);
	globalThis.fetch = vi.fn(() =>
		Promise.resolve({
			ok: status >= 200 && status < 300,
			status,
			body: { getReader: () => reader },
		} as unknown as Response)
	);
	return reader;
}

function setLoggedIn(loggedIn: boolean): void {
	document.cookie = loggedIn
		? 'chv_logged_in=1; Path=/'
		: 'chv_logged_in=; Max-Age=0; Path=/';
}

describe('TaskStreamStore', () => {
	beforeEach(() => {
		vi.restoreAllMocks();
		setLoggedIn(true);
	});

	it('connects to SSE endpoint with the session cookie and resource_kinds filter', async () => {
		const reader = mockFetchWithStream([], 200, true);
		const store = new TaskStreamStore();
		store.connect(['vm', 'node']);

		expect(globalThis.fetch).toHaveBeenCalledWith(
			expect.stringContaining('/v1/tasks/stream?resource_kinds=vm%2Cnode'),
			expect.objectContaining({
				credentials: 'include',
				signal: expect.any(AbortSignal),
			})
		);
		// #660: no bearer token is assembled in the browser.
		const init = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
			.calls[0][1] as RequestInit;
		expect((init.headers as Record<string, string>).Authorization).toBeUndefined();
		expect(store.status).toBe('connecting');

		// Wait for async connect
		await new Promise((r) => setTimeout(r, 10));
		expect(store.status).toBe('open');
		store.disconnect();
	});

	it('falls back to polling when no session hint is present', () => {
		setLoggedIn(false);

		const store = new TaskStreamStore();
		const pollSpy = vi.spyOn(store, 'startPollingFallback');
		store.connect();

		expect(pollSpy).toHaveBeenCalled();
		store.disconnect();
	});

	it('handles task completion and calls handler', async () => {
		const task: TaskUpdate = {
			task_id: 'op-1',
			status: 'Completed',
			summary: 'CreateVm',
			resource_kind: 'vm',
			resource_id: 'vm-123',
			event_unix_ms: Date.now(),
		};
		const payload = JSON.stringify({ items: [task] });
		const encoder = new TextEncoder();
		const reader = mockFetchWithStream([encoder.encode(`data: ${payload}\n\n`)]);

		const handler = vi.fn();
		const store = new TaskStreamStore();
		store.onTaskCompleted = handler;
		store.connect();

		// Wait for async read loop
		await new Promise((r) => setTimeout(r, 50));

		expect(handler).toHaveBeenCalledWith(task);
		store.disconnect();
	});

	it('deduplicates by task_id', async () => {
		const task: TaskUpdate = {
			task_id: 'op-1',
			status: 'Completed',
			summary: 'CreateVm',
			resource_kind: 'vm',
			resource_id: 'vm-123',
			event_unix_ms: Date.now(),
		};
		const payload = JSON.stringify({ items: [task] });
		const encoder = new TextEncoder();
		const reader = mockFetchWithStream([
			encoder.encode(`data: ${payload}\n\n`),
			encoder.encode(`data: ${payload}\n\n`),
		]);

		const handler = vi.fn();
		const store = new TaskStreamStore();
		store.onTaskCompleted = handler;
		store.connect();

		await new Promise((r) => setTimeout(r, 50));

		expect(handler).toHaveBeenCalledTimes(1);
		store.disconnect();
	});

	it('sets error status on 401 and does not reconnect', async () => {
		mockFetchWithStream([], 401);
		const store = new TaskStreamStore();
		const reconnectSpy = vi.spyOn(store as any, 'scheduleReconnect');
		store.connect();

		await new Promise((r) => setTimeout(r, 10));

		expect(store.status).toBe('error');
		expect(reconnectSpy).not.toHaveBeenCalled();
		store.disconnect();
	});

	it('reconnects on non-401 errors with backoff', async () => {
		mockFetchWithStream([], 500);
		const store = new TaskStreamStore();
		const reconnectSpy = vi.spyOn(store as any, 'scheduleReconnect');
		store.connect();

		await new Promise((r) => setTimeout(r, 10));

		expect(store.status).toBe('error');
		expect(reconnectSpy).toHaveBeenCalled();
		store.disconnect();
	});
});
