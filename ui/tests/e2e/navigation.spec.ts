import { test, expect } from '@playwright/test';
import { loginAsAdmin, mockApiResponse } from './helpers';

test.describe('Navigation & Auth', () => {
	test.beforeEach(async ({ page }) => {
		await loginAsAdmin(page);
		await mockApiResponse(page, '**/v1/overview', {
			vms_total: 0, nodes_total: 0, alerts: [], recent_tasks: []
		});
		await page.goto('/');
	});

	test('command palette opens with Ctrl+K keyboard shortcut', async ({ page }) => {
		await page.waitForTimeout(500);
		await page.evaluate(() => {
			window.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', ctrlKey: true, bubbles: true }));
		});
		await expect(page.locator('.fixed.inset-0')).toBeVisible();
		await expect(page.getByPlaceholder(/type a command or search/i)).toBeVisible();
	});

	test('command palette opens via top bar click', async ({ page }) => {
		await page.getByRole('button', { name: /search commands or jump to a resource/i }).click();
		await expect(page.locator('.fixed.inset-0')).toBeVisible();
	});

	test('logout clears the session hint and redirects to login', async ({ page }) => {
		await page.getByRole('button', { name: /sign out/i }).click();
		await expect(page).toHaveURL('/login');
		// #660: no credential is ever stored client-side, and the UX hint is
		// cleared so a reload does not bounce back into the app.
		expect(await page.evaluate(() => localStorage.getItem('chv-api-token'))).toBeNull();
		const cookies = await page.context().cookies();
		expect(cookies.find((c) => c.name === 'chv_logged_in')).toBeUndefined();
		expect(cookies.find((c) => c.name === 'chv_role')).toBeUndefined();
	});

	test('unauthenticated user is redirected to login from protected route', async ({ browser }) => {
		const context = await browser.newContext();
		const newPage = await context.newPage();
		await newPage.goto('/vms');
		await expect(newPage).toHaveURL('/login', { timeout: 10000 });
		await context.close();
	});
});
