import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('$app/env/public', () => ({
	PUBLIC_CHV_API_BASE_URL: ''
}));

import {
	APIError,
	clearToken,
	createAPIClient,
	getStoredRole,
	getStoredToken,
	isLoggedIn,
	storeToken
} from '#lib/api/client.ts';

function jsonHeaders(): Headers {
	return new Headers({ 'content-type': 'application/json' });
}

function setCookie(name: string, value: string): void {
	document.cookie = `${name}=${value}; Path=/`;
}

describe('createAPIClient', () => {
	beforeEach(() => {
		vi.restoreAllMocks();
		// No credential is ever persisted client-side (#660).
		document.cookie = 'chv_api_hint=; Max-Age=0; Path=/';
		localStorage.clear();
		sessionStorage.clear();
	});

	it('uses an explicit base URL and an explicit in-memory token for non-browser clients', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ([])
		});

		vi.stubGlobal('fetch', fetchMock);

		const client = createAPIClient({
			baseUrl: 'http://controller.example'
		});

		client.setToken('token-123');
		await client.listNodes();

		expect(fetchMock).toHaveBeenCalledWith(
			'http://controller.example/api/v1/nodes',
			expect.objectContaining({
				credentials: 'include',
				headers: expect.any(Headers)
			})
		);

		const headers = fetchMock.mock.calls[0][1].headers as Headers;
		expect(headers.get('Authorization')).toBe('Bearer token-123');
		// The token is in-memory only: never localStorage/sessionStorage.
		expect(localStorage.getItem('chv-api-token')).toBeNull();
		expect(sessionStorage.getItem('chv-api-token')).toBeNull();
	});

	it('authenticates the browser with the HttpOnly cookie, not a bearer header', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: jsonHeaders(),
			json: async () => ([])
		});
		vi.stubGlobal('fetch', fetchMock);

		const client = createAPIClient();
		await client.listNodes();

		const init = fetchMock.mock.calls[0][1] as RequestInit;
		expect(init.credentials).toBe('include');
		expect((init.headers as Headers).get('Authorization')).toBeNull();
	});

	it('throws APIError on successful HTML response instead of uncaught JSON parse failure', async () => {
		const fetchMock = vi.fn().mockResolvedValue({
			ok: true,
			status: 200,
			headers: new Headers({ 'content-type': 'text/html' }),
			text: async () => '<!doctype html><html><body>fallback</body></html>',
			json: async () => {
				throw new Error('should not parse');
			}
		});
		vi.stubGlobal('fetch', fetchMock);

		const client = createAPIClient();

		await expect(client.listNodes()).rejects.toBeInstanceOf(APIError);
		await expect(client.listNodes()).rejects.toMatchObject({
			status: 200,
			code: 'INVALID_RESPONSE'
		});
	});
});

describe('cookie-session helpers', () => {
	beforeEach(() => {
		document.cookie = 'chv_role=; Max-Age=0; Path=/';
		document.cookie = 'chv_logged_in=; Max-Age=0; Path=/';
		localStorage.clear();
		sessionStorage.clear();
	});

	it('never returns or stores a bearer token', () => {
		storeToken('header.payload.signature');
		expect(getStoredToken()).toBeNull();
		expect(localStorage.getItem('chv-api-token')).toBeNull();
		expect(sessionStorage.getItem('chv-api-token')).toBeNull();
	});

	it('reads the non-credential role hint cookie', () => {
		setCookie('chv_role', 'admin');
		expect(getStoredRole()).toBe('admin');
		setCookie('chv_role', 'viewer');
		expect(getStoredRole()).toBe('viewer');
	});

	it('returns null when the role hint cookie is absent', () => {
		expect(getStoredRole()).toBeNull();
	});

	it('uses the logged-in hint cookie for the login redirect', () => {
		expect(isLoggedIn()).toBe(false);
		setCookie('chv_logged_in', '1');
		expect(isLoggedIn()).toBe(true);
	});

	it('clearToken removes the hint cookies', () => {
		setCookie('chv_role', 'admin');
		setCookie('chv_logged_in', '1');
		clearToken();
		expect(getStoredRole()).toBeNull();
		expect(isLoggedIn()).toBe(false);
	});
});
