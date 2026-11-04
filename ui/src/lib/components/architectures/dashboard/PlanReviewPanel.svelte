<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import PlanChangeRow from './PlanChangeRow.svelte';
	import PlanTtlBadge from './PlanTtlBadge.svelte';
	import ApplyConfirmDialog from '#lib/components/architectures/runs/ApplyConfirmDialog.svelte';
	import type {
		Architecture,
		PlanChange,
		PlanMode,
		PlanResult
	} from '#lib/bff/architectures.ts';

	interface Props {
		/** Architecture row whose plan is being reviewed. */
		architecture: Architecture;
		/** Latest plan result. `null` means "no plan generated yet". */
		planResult: PlanResult | null;
		/** True while a plan/destroy-plan request is in flight. */
		loading?: boolean;
		/** True while a discard-plan request is in flight. */
		discarding?: boolean;
		/** True while the apply/destroy POST is in flight (Phase 5). */
		applying?: boolean;
		/** Click handler for "Generate plan" / "Generate destroy plan". */
		onGenerate: (mode: PlanMode) => void;
		/** Click handler for "Discard plan" — receives the plan_id. */
		onDiscard: (planId: string) => void;
		/**
		 * Phase 5 apply hook. Receives the typed-name (empty string when not
		 * required) and the warning-acknowledgment flag. Parent owns the
		 * BFF call + post-success navigation.
		 */
		onApply?: (
			planId: string,
			mode: PlanMode,
			typedName: string,
			acknowledgedWarnings: boolean
		) => void;
	}

	let {
		architecture,
		planResult,
		loading = false,
		discarding = false,
		applying = false,
		onGenerate,
		onDiscard,
		onApply
	}: Props = $props();

	const architectureName = $derived(architecture.display_name ?? architecture.name);

	const blocked = $derived(planResult?.status === 'failed_validation');

	const hasDestructiveChanges = $derived(
		planResult?.changes.some((c) => c.requires_confirmation) ?? false
	);

	let dialogOpen = $state(false);

	/**
	 * Group changes by `resource_type`, preserving the wire's apply-order via
	 * Map insertion order (mirrors the server's deterministic ordering).
	 */
	const grouped = $derived.by<{ resource_type: string; changes: PlanChange[] }[]>(() => {
		if (!planResult) return [];
		const groups = new Map<string, PlanChange[]>();
		for (const change of planResult.changes) {
			const list = groups.get(change.resource_type);
			if (list) list.push(change);
			else groups.set(change.resource_type, [change]);
		}
		return Array.from(groups.entries()).map(([resource_type, changes]) => ({
			resource_type,
			changes
		}));
	});

	const generateLabel = $derived(loading ? 'Generating…' : 'Generate plan');
	const destroyLabel = $derived(loading ? 'Generating…' : 'Generate destroy plan');
	const discardLabel = $derived(discarding ? 'Discarding…' : 'Discard plan');

	/**
	 * Apply is enabled when the plan is in the canonical ready-to-apply
	 * status (or its `requires_confirmation` cousin) and the parent supplied
	 * an onApply handler. Expired/discarded/failed plans must regenerate.
	 */
	const applyEnabled = $derived(
		!!onApply &&
			!!planResult &&
			!blocked &&
			!applying &&
			(planResult.status === 'ready_to_apply' ||
				planResult.status === 'requires_confirmation') &&
			new Date(planResult.expires_at).getTime() > Date.now()
	);

	function handleApplyClick() {
		if (!applyEnabled || !planResult) return;
		dialogOpen = true;
	}

	function handleDialogConfirm(typedName: string, acknowledgedWarnings: boolean) {
		if (!planResult || !onApply) return;
		onApply(planResult.plan_id, planResult.mode, typedName, acknowledgedWarnings);
	}

	function handleDialogCancel() {
		dialogOpen = false;
	}
</script>

<section class="panel" aria-label="Plan review" data-testid="plan-review-panel">
	<header class="hdr">
		<div class="hl">
			<h2 class="title">Plan</h2>
			{#if planResult}
				<span
					class="mode mode-{planResult.mode}"
					data-testid="plan-mode-badge"
					aria-label={`Plan mode: ${planResult.mode}`}
				>{planResult.mode}</span>
				<PlanTtlBadge expiresAt={planResult.expires_at} />
			{/if}
		</div>
		<div class="hr">
			<Button
				variant="secondary"
				size="sm"
				loading={loading}
				onclick={() => onGenerate('apply')}
				ariaLabel={generateLabel}
				data-testid="plan-generate-button"
			>{generateLabel}</Button>
			<Button
				variant="danger"
				size="sm"
				loading={loading}
				onclick={() => onGenerate('destroy')}
				ariaLabel={destroyLabel}
				data-testid="plan-destroy-button"
			>{destroyLabel}</Button>
		</div>
	</header>

	{#if planResult === null && !loading}
		<div class="empty" role="status" data-testid="plan-empty">
			<p class="et">No plan has been generated yet.</p>
			<p class="ex">Click <strong>Generate plan</strong> to compute desired changes against the current fleet.</p>
		</div>
	{:else if loading && planResult === null}
		<div class="empty" role="status" data-testid="plan-loading">
			<p class="et">Computing plan…</p>
		</div>
	{:else if planResult !== null}
		{#if blocked}
			<div class="blocked" role="alert" data-testid="plan-blocked-banner">
				<strong>Plan blocked by validation findings.</strong>
				<span>Resolve before generating an apply plan.</span>
				{#if planResult.warnings.length > 0}
					<ul class="wl" data-testid="plan-blocked-warnings">
						{#each planResult.warnings as warning, i (i)}
							<li data-testid="plan-blocked-warning">{warning}</li>
						{/each}
					</ul>
				{/if}
			</div>
		{/if}

		<div class="sum" aria-label="Plan summary" data-testid="plan-summary">
			<span class="chip cc" data-testid="plan-summary-create">Create {planResult.summary.create}</span>
			<span class="chip cu" data-testid="plan-summary-update">Update {planResult.summary.update}</span>
			<span class="chip cr" data-testid="plan-summary-replace">Replace {planResult.summary.replace}</span>
			<span class="chip cd" data-testid="plan-summary-delete">Delete {planResult.summary.delete}</span>
			<span class="chip cw" data-testid="plan-summary-warnings">Warnings {planResult.summary.warnings}</span>
		</div>

		{#if planResult.changes.length === 0 && !blocked}
			<div class="empty" role="status" data-testid="plan-no-changes">
				<p class="et">No changes — the topology already matches the fleet.</p>
			</div>
		{:else if planResult.changes.length > 0}
			<div class="changes" data-testid="plan-changes">
				{#each grouped as group (group.resource_type)}
					<div class="cg">
						<h3 class="gt">{group.resource_type}</h3>
						<ul class="cl">
							{#each group.changes as change (change.resource_ref + change.action)}
								<PlanChangeRow {change} />
							{/each}
						</ul>
					</div>
				{/each}
			</div>
		{/if}

		<footer class="ft">
			<Button
				variant={planResult.mode === 'destroy' ? 'danger' : 'primary'}
				size="sm"
				disabled={!applyEnabled}
				loading={applying}
				onclick={handleApplyClick}
				ariaLabel={planResult.mode === 'destroy' ? 'Apply destroy plan' : 'Apply plan'}
				data-testid="plan-apply-button"
				title={!applyEnabled && !blocked ? 'Plan is not in a ready-to-apply state' : undefined}
			>{planResult.mode === 'destroy' ? 'Apply destroy' : 'Apply plan'}</Button>
			<Button
				variant="ghost"
				size="sm"
				loading={discarding}
				disabled={discarding}
				onclick={() => onDiscard(planResult.plan_id)}
				ariaLabel="Discard plan"
				data-testid="plan-discard-button"
			>{discardLabel}</Button>
		</footer>
	{/if}
</section>

{#if planResult}
	<ApplyConfirmDialog
		bind:open={dialogOpen}
		architectureName={architectureName}
		planMode={planResult.mode}
		warnings={planResult.warnings}
		hasDestructiveChanges={hasDestructiveChanges}
		submitting={applying}
		onConfirm={handleDialogConfirm}
		onCancel={handleDialogCancel}
	/>
{/if}

<style>
	.panel { display: flex; flex-direction: column; gap: 0.75rem; padding: 1rem;
		background: var(--bg-surface); border: 1px solid var(--color-neutral-200);
		border-radius: var(--radius-sm); }
	.hdr { display: flex; justify-content: space-between; align-items: center;
		gap: 0.75rem; flex-wrap: wrap; }
	.hl, .hr { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
	.title { font-size: var(--text-sm); font-weight: 700; margin: 0;
		color: var(--color-neutral-700); text-transform: uppercase; letter-spacing: 0.04em; }
	.mode { display: inline-block; padding: 0.1rem 0.5rem; font-size: 11px;
		font-weight: 700; text-transform: uppercase; letter-spacing: 0.04em;
		border-radius: var(--radius-xs); color: white; }
	.mode-apply { background: #1d4ed8; }
	.mode-destroy { background: #b91c1c; }
	.blocked { display: flex; flex-direction: column; gap: 0.25rem;
		padding: 0.6rem 0.85rem; background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4); border-radius: var(--radius-xs);
		color: rgb(153, 27, 27); font-size: var(--text-sm); }
	.wl { margin: 0.25rem 0 0 0; padding-left: 1.1rem; font-size: 12px; }
	.sum { display: flex; flex-wrap: wrap; gap: 0.4rem; font-size: 12px; }
	.chip { display: inline-block; padding: 0.15rem 0.55rem;
		border-radius: var(--radius-xs); color: white; font-weight: 600;
		letter-spacing: 0.02em; }
	.cc { background: #15803d; }
	.cu { background: #b45309; }
	.cr { background: #c2410c; }
	.cd { background: #b91c1c; }
	.cw { background: #6b7280; }
	.empty { padding: 1rem; border: 1px dashed var(--color-neutral-300);
		border-radius: var(--radius-xs); text-align: center;
		background: var(--color-neutral-50, #f8fafc); }
	.et { margin: 0; font-size: var(--text-sm); font-weight: 600; color: var(--color-neutral-700); }
	.ex { margin: 0.25rem 0 0 0; font-size: 12px; color: var(--color-neutral-600); }
	.changes { display: flex; flex-direction: column; gap: 0.6rem; }
	.cg { display: flex; flex-direction: column; gap: 0.25rem; }
	.gt { margin: 0; font-size: 12px; font-weight: 700; text-transform: uppercase;
		letter-spacing: 0.04em; color: var(--color-neutral-600); }
	.cl { list-style: none; margin: 0; padding: 0; display: flex;
		flex-direction: column; gap: 0.25rem; }
	.ft { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
</style>
