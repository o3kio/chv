import { test, expect, type Page } from '@playwright/test';
import { loginAsAdmin, mockApiResponse } from './helpers';

/**
 * Native alerting surface (query/alerts contract v1, #602 PR-6).
 *
 * Wire shape under test (handlers/alerts.rs):
 *   - POST /v1/monitoring/alerts                    -> incidents
 *   - POST /v1/monitoring/alerts/detail             -> incident + transitions
 *   - POST /v1/monitoring/alert-rules               -> rules (flat spec)
 *   - POST /v1/monitoring/notifications/deliveries  -> delivery audit
 *
 * The page must render state/severity badges, ack/silence overlays,
 * spec summaries, and the create-from-template dialog whose starter
 * templates never arrive enabled.
 */

const NOW = Date.now();

function incidentRow(overrides: Record<string, unknown> = {}) {
	return {
		alert_id: 'alert-1',
		status: 'firing',
		severity: 'critical',
		rule_id: 'rule-1',
		rule_revision: 2,
		dedup_key: 'rule-1:vm:vm-1:-',
		target_kind: 'vm',
		target_id: 'vm-1',
		node_id: 'node-1',
		message: 'vm.cpu.capacity_ratio sustained above 0.9',
		last_observed: '0.94 (vm.cpu.capacity_ratio)',
		opened_at: new Date(NOW - 3_600_000).toISOString(),
		acknowledged_at: null,
		acknowledged_by: null,
		silenced_until_ms: null,
		silenced_by: null,
		pending_since_ms: NOW - 3_900_000,
		first_occurrence_ms: NOW - 3_900_000,
		last_occurrence_ms: NOW - 15_000,
		evidence_from_ms: NOW - 3_600_000,
		evidence_to_ms: NOW - 15_000,
		resolved_at: null,
		...overrides
	};
}

function ruleRow(overrides: Record<string, unknown> = {}) {
	return {
		rule_id: 'rule-1',
		name: 'VM CPU pressure',
		enabled: true,
		target_kind: 'vm',
		target_id: 'vm-1',
		rule_type: 'threshold',
		severity: 'warning',
		for_seconds: 300,
		recovery_seconds: 120,
		missing_data: 'unknown',
		revision: 3,
		created_by: 'admin',
		created_at_ms: NOW - 86_400_000,
		updated_at_ms: NOW - 3_600_000,
		metric_id: 'vm.cpu.capacity_ratio',
		operator: 'greater_than',
		threshold: 0.9,
		...overrides
	};
}

/** The role hint cookie must carry an operator/admin role. */
async function loginAsOperator(page: Page) {
	await page.context().addCookies([
		{ name: 'chv_logged_in', value: '1', domain: 'localhost', path: '/', sameSite: 'Strict' },
		{ name: 'chv_role', value: 'admin', domain: 'localhost', path: '/', sameSite: 'Strict' }
	]);
}

async function mockAlerting(
	page: Page,
	overrides: {
		incidents?: unknown;
		rules?: unknown;
		deliveries?: unknown;
	} = {}
) {
	await mockApiResponse(page, '**/v1/monitoring/alerts', overrides.incidents ?? { incidents: [], total: 0 });
	await mockApiResponse(page, '**/v1/monitoring/alerts/detail', {
		incident: incidentRow(),
		transitions: [
			{
				from_state: null,
				to_state: 'pending',
				occurred_at_ms: NOW - 3_900_000,
				reason: 'condition observed',
				measured: '0.93 (vm.cpu.capacity_ratio)'
			},
			{
				from_state: 'pending',
				to_state: 'firing',
				occurred_at_ms: NOW - 3_600_000,
				reason: 'hold met',
				measured: '0.94 (vm.cpu.capacity_ratio)'
			}
		]
	});
	await mockApiResponse(page, '**/v1/monitoring/alert-rules', overrides.rules ?? { rules: [], total: 0 });
	await mockApiResponse(page, '**/v1/monitoring/notifications/deliveries', overrides.deliveries ?? { deliveries: [] });
}

test.beforeEach(async ({ page }) => {
	await loginAsOperator(page);
});

test('renders firing incidents with overlays and the delivery audit', async ({ page }) => {
	await mockAlerting(page, {
		incidents: { incidents: [incidentRow()], total: 1 },
		deliveries: {
			deliveries: [
				{
					event_id: 'event-1',
					alert_id: 'alert-1',
					event_type: 'firing',
					severity: 'critical',
					target_kind: 'vm',
					target_id: 'vm-1',
					summary: 'VM CPU pressure firing',
					channel: 'webhook',
					status: 'delivered',
					attempts: 1,
					next_attempt_at_ms: NOW,
					last_attempt_ms: NOW - 1000,
					last_response: '2xx',
					occurred_at_ms: NOW - 3_600_000,
					updated_at_ms: NOW - 3_600_000
				}
			]
		}
	});
	await page.goto('/alerts');

	await expect(page.getByRole('tab', { name: /incidents/i })).toBeVisible();
	await expect(page.getByText('Firing', { exact: true })).toBeVisible();
	await expect(page.getByText('vm.cpu.capacity_ratio sustained above 0.9')).toBeVisible();
	await expect(page.getByText('0.94 (vm.cpu.capacity_ratio)')).toBeVisible();
	// Operator-gated actions are visible for an admin token.
	await expect(page.getByRole('button', { name: 'Acknowledge incident' })).toBeVisible();
	// Delivery audit renders the outbox row.
	await expect(page.getByText('Recent deliveries')).toBeVisible();
	await expect(page.getByText('VM CPU pressure firing')).toBeVisible();
});

test('incident row opens the detail drawer with its transition timeline', async ({ page }) => {
	await mockAlerting(page, { incidents: { incidents: [incidentRow()], total: 1 } });
	await page.goto('/alerts');

	await page.getByText('vm.cpu.capacity_ratio sustained above 0.9').click();
	const drawer = page.locator('[role="dialog"]');
	await expect(drawer.getByText('Incident detail')).toBeVisible();
	await expect(drawer.getByText('created → pending')).toBeVisible();
	await expect(drawer.getByText('pending → firing')).toBeVisible();
	await expect(drawer.getByText(/hold met/)).toBeVisible();
});

test('show resolved toggle refetches with resolved incidents included', async ({ page }) => {
	await mockAlerting(page);
	// Registered AFTER mockAlerting so it takes precedence for the
	// incidents path (the last matching route wins in Playwright).
	let includeResolvedSeen: boolean | undefined;
	await page.route('**/v1/monitoring/alerts', async (route) => {
		const body = route.request().postDataJSON() as { include_resolved?: boolean };
		includeResolvedSeen = body.include_resolved;
		await route.fulfill({
			status: 200,
			contentType: 'application/json',
			body: JSON.stringify({
				incidents: [
					incidentRow({
						alert_id: 'alert-2',
						status: 'resolved',
						severity: 'warning',
						resolved_at: new Date(NOW - 600_000).toISOString()
					})
				],
				total: 1
			})
		});
	});
	await page.goto('/alerts');

	await page.getByLabel('Show resolved').click();
	await expect(page.getByText('Resolved', { exact: true })).toBeVisible();
	expect(includeResolvedSeen).toBe(true);
});

test('rules tab renders spec summaries and never auto-enables a new rule', async ({ page }) => {
	await mockAlerting(page, { rules: { rules: [ruleRow()], total: 1 } });
	await page.goto('/alerts?tab=rules');

	await expect(page.getByText('VM CPU pressure', { exact: true })).toBeVisible();
	await expect(page.getByText('vm.cpu.capacity_ratio > 0.9')).toBeVisible();
	await expect(page.getByText(/rev 3/)).toBeVisible();

	// The create dialog offers the seven starter templates and starts
	// disabled — the operator must review and flip the switch.
	await page.getByRole('button', { name: 'New rule' }).click();
	const dialog = page.locator('[role="dialog"]');
	await expect(dialog.getByText('Start from a template')).toBeVisible();
	for (const title of [
		'Node unreachable',
		'Collector stale',
		'VM CPU pressure',
		'VM storage near full',
		'Agent disconnected',
		'Guest filesystem full (inodes)',
		'Guest service down'
	]) {
		// The template buttons carry the title plus the rule type, so
		// match on the title prefix rather than the exact name.
		const escaped = title.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
		await expect(dialog.getByRole('button', { name: new RegExp(`^${escaped}`) })).toBeVisible();
	}
	const enabledCheckbox = dialog.getByLabel(/enabled/i);
	await expect(enabledCheckbox).not.toBeChecked();
});

test('rules tab renders group summaries and the multi-key dimension hint', async ({ page }) => {
	// A group rule's nested conditions ride the wire UNTAGGED (no
	// rule_type on children); the summary must shape-sniff them. A
	// two-key dimension match cannot be edited down to one key
	// silently — the edit dialog says what untouched vs edited fields
	// do.
	await mockAlerting(page, {
		rules: {
			rules: [
				ruleRow({
					rule_id: 'rule-group',
					name: 'Node pressure combo',
					rule_type: 'group',
					op: 'and',
					conditions: [
						{
							rule_id: 'c1',
							metric_id: 'node.cpu.load1',
							operator: 'greater_than',
							threshold: 8
						},
						{
							rule_id: 'c2',
							check_id: 'service:nginx.service',
							status_match: 'critical'
						}
					]
				}),
				ruleRow({
					rule_id: 'rule-legacy',
					name: 'Legacy two-key match',
					dimension_match: { mount_id: 'ext4:/', block_device_id: 'sda' }
				})
			],
			total: 2
		}
	});
	await page.goto('/alerts?tab=rules');

	// The group summary renders every child via shape sniffing.
	await expect(
		page.getByText('node.cpu.load1 > 8 AND service:nginx.service is critical')
	).toBeVisible();

	// Editing the two-key rule shows the honest note.
	await page.getByRole('button', { name: 'Edit rule' }).last().click();
	const dialog = page.locator('[role="dialog"]');
	await expect(dialog.getByText('Edit alert rule')).toBeVisible();
	await expect(
		dialog.getByText(/matches 2 dimensions \(mount_id, block_device_id\)/)
	).toBeVisible();
});

test('rule update conflict shows the reload notice', async ({ page }) => {
	await mockAlerting(page, { rules: { rules: [ruleRow()], total: 1 } });
	await page.goto('/alerts?tab=rules');

	await page.getByRole('button', { name: 'Edit rule' }).click();
	const dialog = page.locator('[role="dialog"]');
	await expect(dialog.getByText('Edit alert rule')).toBeVisible();

	// The next update answers 409 — someone else changed the rule.
	await page.route('**/v1/monitoring/alert-rules/update', async (route) => {
		await route.fulfill({
			status: 409,
			contentType: 'application/json',
			body: JSON.stringify({ message: 'revision mismatch', code: 'CONFLICT' })
		});
	});
	await dialog.getByRole('button', { name: 'Save rule' }).click();

	await expect(page.getByText(/modified by someone else/i)).toBeVisible();
	await expect(dialog).toBeHidden();
});

test('viewer sessions see no operator actions', async ({ page }) => {
	// The role hint is a viewer, exactly the read-only path the server gates.
	await page.context().addCookies([
		{ name: 'chv_logged_in', value: '1', domain: 'localhost', path: '/', sameSite: 'Strict' },
		{ name: 'chv_role', value: 'viewer', domain: 'localhost', path: '/', sameSite: 'Strict' }
	]);
	await mockAlerting(page, { incidents: { incidents: [incidentRow()], total: 1 } });
	await page.goto('/alerts');

	await expect(page.getByRole('button', { name: 'Acknowledge incident' })).toHaveCount(0);
	await page.getByRole('tab', { name: /rules/i }).click();
	await expect(page.getByRole('button', { name: 'New rule' })).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Edit rule' })).toHaveCount(0);
});
