<script lang="ts">
	import { Plus, Pencil, Trash2, AlertTriangle } from 'lucide-svelte';
	import Badge from '#lib/components/primitives/Badge.svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import ErrorState from '#lib/components/shell/ErrorState.svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import {
		createRule,
		deleteRule,
		updateRule,
		type AlertRule,
		type RuleMutateBody
	} from '#lib/bff/alerting.ts';
	import { BFFError } from '#lib/bff/client.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import {
		formatAlertTimestamp,
		ruleSpecSummary,
		severityBadgeVariant,
		targetHref
	} from '#lib/alerts/rules.ts';
	import RuleEditorDialog from './RuleEditorDialog.svelte';

	interface Props {
		rules: AlertRule[];
		total: number;
		loading: boolean;
		error: boolean;
		isOperator: boolean;
		onReload: () => Promise<void>;
	}

	let { rules, total, loading, error, isOperator, onReload }: Props = $props();

	let dialogOpen = $state(false);
	let editingRule = $state<AlertRule | null>(null);
	let busyRuleId = $state<string | null>(null);
	/** Set when a mutation hit a 409 revision conflict. */
	let conflictNotice = $state('');

	function openCreate() {
		editingRule = null;
		conflictNotice = '';
		dialogOpen = true;
	}

	function openEdit(rule: AlertRule) {
		editingRule = rule;
		conflictNotice = '';
		dialogOpen = true;
	}

	async function submitRule(body: RuleMutateBody): Promise<void> {
		const editing = editingRule !== null;
		try {
			await mutateWithRefresh(
				() =>
					editing
						? updateRule(body, getStoredToken() ?? undefined)
						: createRule(body, getStoredToken() ?? undefined),
				{
					skipRefresh: true,
					successMessage: editing ? 'Rule updated' : 'Rule created (disabled until you enable it)',
					errorMessage: editing ? 'Failed to update rule' : 'Failed to create rule'
				}
			);
			await onReload();
		} catch (err) {
			if (err instanceof BFFError && err.status === 409) {
				// 409 on update: revision conflict (someone else changed
				// the rule) — reload the current revision. 409 on create:
				// there is no revision yet; the only create-time conflict
				// is the configured rule ceiling.
				conflictNotice = editing
					? 'Rule was modified by someone else — reloading the current revision.'
					: 'Rule ceiling reached — delete unused rules or raise monitoring.alerting.max_rules.';
				await onReload();
				return;
			}
			throw err;
		}
	}

	async function toggleEnabled(rule: AlertRule) {
		const ruleId = rule.rule_id;
		busyRuleId = ruleId;
		try {
			await mutateWithRefresh(
				() =>
					updateRule(
						{
							rule_id: rule.rule_id,
							expected_revision: rule.revision,
							name: rule.name,
							target_kind: rule.target_kind,
							target_id: rule.target_id,
							severity: rule.severity,
							for_seconds: rule.for_seconds,
							recovery_seconds: rule.recovery_seconds,
							missing_data: rule.missing_data,
							enabled: !rule.enabled,
							metric_id: rule.metric_id,
							dimension_match: rule.dimension_match,
							operator: rule.operator,
							threshold: rule.threshold,
							threshold_per_second: rule.threshold_per_second,
							window_seconds: rule.window_seconds,
							check_id: rule.check_id,
							status_match: rule.status_match,
							op: rule.op,
							conditions: rule.conditions
						},
						getStoredToken() ?? undefined
					),
				{
					skipRefresh: true,
					successMessage: rule.enabled ? 'Rule disabled' : 'Rule enabled',
					errorMessage: 'Failed to toggle rule'
				}
			);
			await onReload();
		} catch (err) {
			if (err instanceof BFFError && err.status === 409) {
				conflictNotice = 'Rule was modified by someone else — reloading the current revision.';
			}
			await onReload();
		} finally {
			busyRuleId = null;
		}
	}

	async function removeRule(rule: AlertRule) {
		if (
			!window.confirm(
				`Delete rule "${rule.name}"? Its firing incidents will be resolved (with a final notification); never-fired pending incidents are deleted.`
			)
		) {
			return;
		}
		const ruleId = rule.rule_id;
		busyRuleId = ruleId;
		try {
			await mutateWithRefresh(
				() =>
					deleteRule(
						{ rule_id: rule.rule_id, expected_revision: rule.revision },
						getStoredToken() ?? undefined
					),
				{
					skipRefresh: true,
					successMessage: 'Rule deleted',
					errorMessage: 'Failed to delete rule'
				}
			);
			await onReload();
		} catch (err) {
			if (err instanceof BFFError && err.status === 409) {
				conflictNotice = 'Rule was modified by someone else — reloading the current revision.';
			}
			await onReload();
		} finally {
			busyRuleId = null;
		}
	}
</script>

<SectionCard title="Rules" badgeLabel={String(total)}>
	{#snippet actions()}
		{#if isOperator}
			<Button variant="primary" size="sm" onclick={openCreate}>
				<Plus size={14} />
				New rule
			</Button>
		{/if}
	{/snippet}

	{#if conflictNotice}
		<div
			class="flex items-center gap-2 mb-3 p-2 rounded-xs border border-warning/30 bg-warning/10 text-xs text-(--color-warning-dark)"
			role="alert"
		>
			<AlertTriangle size={14} />
			{conflictNotice}
		</div>
	{/if}

	{#if error}
		<ErrorState />
	{:else if loading && rules.length === 0}
		<p class="text-sm text-(--shell-text-muted)">Loading rules…</p>
	{:else if rules.length === 0}
		<p class="text-sm text-(--shell-text-secondary)">
			No alert rules yet. Start from one of the seven starter templates — created rules stay
			disabled until you review and enable them.
		</p>
	{:else}
		<div class="overflow-x-auto">
			<table class="w-full text-xs border-collapse">
				<thead>
					<tr class="text-left text-(--shell-text-muted) border-b border-(--shell-line)">
						<th class="py-1.5 pr-3 font-semibold">Enabled</th>
						<th class="py-1.5 pr-3 font-semibold">Name</th>
						<th class="py-1.5 pr-3 font-semibold">Type</th>
						<th class="py-1.5 pr-3 font-semibold">Severity</th>
						<th class="py-1.5 pr-3 font-semibold">Target</th>
						<th class="py-1.5 pr-3 font-semibold">Spec</th>
						<th class="py-1.5 pr-3 font-semibold">Updated</th>
						{#if isOperator}
							<th class="py-1.5 font-semibold text-right">Actions</th>
						{/if}
					</tr>
				</thead>
				<tbody>
					{#each rules as rule (rule.rule_id)}
						<tr class="border-b border-(--shell-line) last:border-b-0">
							<td class="py-1.5 pr-3 whitespace-nowrap">
								{#if isOperator}
									<input
										type="checkbox"
										checked={rule.enabled}
										disabled={busyRuleId === rule.rule_id}
										onchange={() => toggleEnabled(rule)}
										aria-label="Toggle rule enabled"
									/>
								{:else}
									<Badge variant={rule.enabled ? 'success' : 'default'} dot>
										{rule.enabled ? 'on' : 'off'}
									</Badge>
								{/if}
							</td>
							<td class="py-1.5 pr-3 font-medium text-(--shell-text)">{rule.name}</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) whitespace-nowrap">
								{rule.rule_type}
								<span class="text-(--shell-text-muted)"> · rev {rule.revision}</span>
							</td>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<Badge variant={severityBadgeVariant(rule.severity)}>{rule.severity}</Badge>
							</td>
							<td class="py-1.5 pr-3 whitespace-nowrap">
								<a
									href={targetHref(rule.target_kind, rule.target_id)}
									class="text-(--shell-accent) no-underline hover:underline"
								>
									{rule.target_kind}/{rule.target_id}
								</a>
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) max-w-[24rem] truncate" title={ruleSpecSummary(rule)}>
								{ruleSpecSummary(rule)}
							</td>
							<td class="py-1.5 pr-3 text-(--shell-text-secondary) whitespace-nowrap">
								{formatAlertTimestamp(rule.updated_at_ms)}
							</td>
							{#if isOperator}
								<td class="py-1.5 text-right whitespace-nowrap">
									<span class="inline-flex items-center gap-1">
										<Button
											variant="ghost"
											size="sm"
											ariaLabel="Edit rule"
											title="Edit"
											disabled={busyRuleId === rule.rule_id}
											onclick={() => openEdit(rule)}
										>
											<Pencil size={13} />
										</Button>
										<Button
											variant="ghost"
											size="sm"
											ariaLabel="Delete rule"
											title="Delete"
											disabled={busyRuleId === rule.rule_id}
											onclick={() => removeRule(rule)}
										>
											<Trash2 size={13} />
										</Button>
									</span>
								</td>
							{/if}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</SectionCard>

{#if isOperator}
	<RuleEditorDialog bind:open={dialogOpen} rule={editingRule} onClose={() => (dialogOpen = false)} onSubmit={submitRule} />
{/if}
