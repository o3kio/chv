import { test, expect } from '@playwright/test';
import { loginAsAdmin, mockApiResponse, waitForHydration } from './helpers';

test.describe('Authentication', () => {
	test('has expected login form', async ({ page }) => {
		await page.goto('/login');

		await expect(page.getByLabel(/operator identity/i)).toBeVisible();
		await expect(page.getByLabel(/access credential/i)).toBeVisible();
		await expect(page.getByRole('button', { name: /authenticate session/i })).toBeVisible();
	});

	test('shows error on invalid credentials', async ({ page }) => {
		await page.goto('/login');
		await waitForHydration(page);
		await mockApiResponse(page, '/v1/auth/login', { error: { message: 'Invalid credentials' } }, 401);

		await page.getByLabel(/operator identity/i).fill('admin');
		await page.getByLabel(/access credential/i).fill('wrong-password');
		await page.getByRole('button', { name: /authenticate session/i }).click();

		await expect(page.locator('.login-error')).toContainText('Invalid credentials');
	});

	test('successful login redirects and stores no token client-side', async ({ page }) => {
		await page.goto('/login');
		await waitForHydration(page);
		// The browser login no longer returns a JWT (#660); only the server's
		// HttpOnly cookie carries the credential. The hint cookies are
		// non-credential UX state the SPA can read. Mirror the BFF by setting
		// them on the login response rather than pre-seeding them: seeding
		// them before the page hydrates races the login page's onMount
		// session probe, which redirects when /v1/overview answers OK.
		await mockApiResponse(
			page,
			'/v1/auth/login',
			{
				user: { username: 'admin', role: 'admin' },
				must_change_password: false
			},
			200,
			{
				'set-cookie':
					'chv_logged_in=1; Path=/; SameSite=Strict\nchv_role=admin; Path=/; SameSite=Strict'
			}
		);

		await page.getByLabel(/operator identity/i).fill('admin');
		await page.getByLabel(/access credential/i).fill('correct-password');
		await page.getByRole('button', { name: /authenticate session/i }).click();

		await expect(page).toHaveURL('/');

		// Regression guard for #660: no credential may land in web storage.
		expect(await page.evaluate(() => localStorage.getItem('chv-api-token'))).toBeNull();
		expect(
			await page.evaluate(() =>
				Object.keys(localStorage).filter((k) => k.toLowerCase().includes('token'))
			)
		).toEqual([]);
		expect(
			await page.evaluate(() =>
				Object.keys(sessionStorage).filter((k) => k.toLowerCase().includes('token'))
			)
		).toEqual([]);
	});

	test('session hint cookie allows direct access to dashboard', async ({ page }) => {
		// Seed the hint before navigating so the test models arriving with an
		// existing session rather than racing the login page's onMount probe.
		await loginAsAdmin(page);
		await page.goto('/');
		await expect(page.getByText('Fleet Overview')).toBeVisible();
	});
});
