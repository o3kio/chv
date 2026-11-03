import { test, expect, type Page } from '@playwright/test';

/**
 * Real-BFF flow (#659).
 *
 * Every other Playwright spec stubs /v1/** with `page.route` or the mock BFF,
 * which means none of them can catch a serde field rename between the Rust
 * BFF and the SvelteKit parsers. This spec talks to an actual
 * `chv-controlplane` process:
 *
 *   scripts/integration/real-bff-e2e.sh
 *     -> disposable SQLite + migrations
 *     -> seeded admin (bcrypt, must_change_password=0)
 *     -> REAL_BFF_BASE_URL / REAL_BFF_E2E_USER / REAL_BFF_E2E_PASSWORD
 *
 * It is skipped when REAL_BFF_BASE_URL is unset, so the fast mock-BFF suite
 * and local `npm run test:e2e` keep working without a Rust build.
 */

const REAL_BFF = process.env.REAL_BFF_BASE_URL ?? '';
const USERNAME = process.env.REAL_BFF_E2E_USER ?? 'admin';
const PASSWORD = process.env.REAL_BFF_E2E_PASSWORD ?? '';

/**
 * The UI is a static build, so its `/v1/**` fetches hit the preview origin.
 * Forward them to the real BFF without changing method/body/headers; the
 * browser still executes the real UI parsers on the real response bytes.
 */
async function forwardToRealBff(page: Page): Promise<void> {
	await page.route('**/v1/**', async (route) => {
		const incoming = new URL(route.request().url());
		// SSE endpoints (e.g. /v1/tasks/stream) are never-ending responses;
		// route.fetch would buffer them forever and hang test teardown. The
		// dashboard assertions do not depend on live events, so return a
		// clean closed stream.
		const accept = route.request().headers()['accept'] ?? '';
		if (accept.includes('text/event-stream')) {
			await route.fulfill({
				status: 200,
				headers: { 'content-type': 'text/event-stream' },
				body: ''
			});
			return;
		}
		const response = await route.fetch({
			url: `${REAL_BFF}${incoming.pathname}${incoming.search}`
		});
		await route.fulfill({ response });
	});
}

test.describe('Real Rust BFF', () => {
	test.skip(
		!REAL_BFF || !PASSWORD,
		'REAL_BFF_BASE_URL/REAL_BFF_E2E_PASSWORD unset — runs in the dedicated real-BFF CI job'
	);

	test('operator signs in and the dashboard parses the real overview response', async ({ page }) => {
		await forwardToRealBff(page);

		await page.goto('/login');
		await page.getByLabel(/operator identity/i).fill(USERNAME);
		await page.getByLabel(/access credential/i).fill(PASSWORD);
		await page.getByRole('button', { name: /authenticate session/i }).click();

		await expect(page).toHaveURL('/');

		// #660: the JWT is minted by the Rust BFF (HS256) but lives only in
		// the HttpOnly `chv_session` cookie, never in script-readable storage.
		const cookies = await page.context().cookies();
		const session = cookies.find((cookie) => cookie.name === 'chv_session');
		expect(session).toBeDefined();
		expect(session?.value.split('.')).toHaveLength(3);
		expect(session?.httpOnly).toBe(true);
		expect(await page.evaluate(() => localStorage.getItem('chv-api-token'))).toBeNull();

		// The seeded database is empty, so a correct real-BFF response drives
		// the honest empty state. A serde drift or a failed /v1/overview call
		// renders the "Telemetry Failure" error state instead. Wait for the
		// loading state to settle first: asserting count(0) while the panel is
		// still loading would pass without proving the real response parsed.
		await expect(page.getByText('Indexing topology...')).toHaveCount(0);
		await expect(page.getByText('Fleet Overview')).toBeVisible();
		await expect(page.getByText('Empty Fleet')).toBeVisible();
		await expect(page.getByText('Telemetry Failure')).toHaveCount(0);
	});

	test('real BFF serializes the documented overview contract as JSON', async ({ request }) => {
		const login = await request.post(`${REAL_BFF}/v1/auth/login`, {
			data: { username: USERNAME, password: PASSWORD }
		});
		expect(login.status()).toBe(200);
		const setCookies = login
			.headersArray()
			.filter((header) => header.name.toLowerCase() === 'set-cookie')
			.map((header) => header.value);
		const sessionCookie = setCookies.find((cookie) => cookie.startsWith('chv_session='));
		if (!sessionCookie) throw new Error('login did not set a chv_session cookie');
		const loginBody = await login.json();
		// #660: the credential is cookie-only; the body must not leak it.
		expect(loginBody.token).toBeUndefined();

		// Authorizing from the HttpOnly session cookie (no Authorization
		// header) proves the cookie channel works for real API clients.
		const overview = await request.post(`${REAL_BFF}/v1/overview`, {
			headers: { Cookie: sessionCookie.split(';', 1)[0] },
			data: {}
		});
		expect(overview.status()).toBe(200);
		expect(overview.headers()['content-type']).toContain('application/json');

		const body = await overview.json();
		expect(typeof body.nodes_total).toBe('number');
		expect(typeof body.vms_total).toBe('number');
		expect(typeof body.active_tasks).toBe('number');
		expect(Array.isArray(body.alerts)).toBe(true);
		expect(Array.isArray(body.recent_tasks)).toBe(true);
	});

	test('real BFF fails closed on invalid credentials', async ({ page, request }) => {
		// API-level: the real auth route returns 401, not a synthetic page.
		const response = await request.post(`${REAL_BFF}/v1/auth/login`, {
			data: { username: USERNAME, password: `${PASSWORD}-wrong` }
		});
		expect(response.status()).toBe(401);

		// UI-level: the operator stays on the login gate and never reaches the
		// overview panel. Waiting for the visible login error first makes the
		// negative dashboard assertion meaningful instead of racing the app.
		await forwardToRealBff(page);
		await page.goto('/login');
		await page.getByLabel(/operator identity/i).fill(USERNAME);
		await page.getByLabel(/access credential/i).fill(`${PASSWORD}-wrong`);
		await page.getByRole('button', { name: /authenticate session/i }).click();

		await expect(page.locator('.login-error')).toBeVisible();
		await expect(page).toHaveURL('/login');
		await expect(page.getByText('Empty Fleet')).toHaveCount(0);
		await expect(page.getByText('Telemetry Failure')).toHaveCount(0);
	});
});
