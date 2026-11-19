<script lang="ts">
	import type { NetboxRunDetail } from '#lib/bff/architectures.ts';
	import {
		NETBOX_ACTION_LABELS,
		NETBOX_PLAN_SUMMARY_CHIPS,
		viewNetboxExecutedPlanSummary,
		viewNetboxRunResult
	} from './types.ts';

	/**
	 * Full view of one selected projection run: the run facts, the abort
	 * error when one occurred, the executed plan's summary counts, and
	 * the per-entry outcomes from `result_json` (parsed payload, or the
	 * raw column string for rows outside the adapter's outcome
	 * contract). The executed-plan chips come from the outcome's
	 * `plan.summary` — the real data flow, since `plan_json` is null in
	 * the current worker — with the top-level `plan_json` column as a
	 * defensive fallback. Extracted from NetboxRunHistory to keep both
	 * under the 300-line component cap.
	 */

	interface Props {
		run: NetboxRunDetail;
		loading: boolean;
	}

	let { run, loading }: Props = $props();

	const statusLabels: Record<string, string> = {
		succeeded: 'Succeeded',
		failed: 'Failed',
		skipped: 'Skipped',
		not_attempted: 'Not attempted'
	};

	const triggerLabels: Record<string, string> = {
		manual: 'Manual',
		post_apply: 'Post-apply'
	};

	function formatTime(iso: string | null): string {
		if (!iso) return '—';
		return new Date(iso).toLocaleString();
	}

	const resultView = $derived(viewNetboxRunResult(run.result_json));
	// The executed plan's counts come from the outcome inside
	// result_json (the real worker flow — plan_json is never written
	// today); the top-level plan_json path stays as the defensive
	// fallback. Null when neither source parses — the executed-plan row
	// is skipped entirely in that case.
	const planSummary = $derived(viewNetboxExecutedPlanSummary(resultView, run.plan_json));
</script>

<section class="detail" aria-label={`NetBox run ${run.id} details`} data-testid="netbox-run-detail">
	<header class="detail-header">
		<h3 class="detail-title">Run {run.id}</h3>
		<span class="detail-meta">
			{triggerLabels[run.trigger] ?? run.trigger} ·
			{run.mode === 'dry_run' ? 'Dry run' : 'Export'} ·
			attempt {run.attempt_count}
		</span>
	</header>
	<dl class="detail-facts">
		<div>
			<dt>Started</dt>
			<dd>{formatTime(run.started_at)}</dd>
		</div>
		<div>
			<dt>Finished</dt>
			<dd>{formatTime(run.finished_at)}</dd>
		</div>
		<div>
			<dt>Applied version</dt>
			<dd>{run.architecture_version_id}</dd>
		</div>
		{#if run.resolved_architecture_version_id}
			<div>
				<dt>Projected version</dt>
				<dd data-testid="netbox-run-resolved-version">
					{run.resolved_architecture_version_id}
					{#if run.resolved_architecture_version_id !== run.architecture_version_id}
						<span class="resolved-note" title="A newer apply succeeded between enqueue and execution; the worker re-resolved at run time">
							(re-resolved)
						</span>
					{/if}
				</dd>
			</div>
		{/if}
	</dl>
	{#if run.error_message}
		<div class="detail-error" role="alert" data-testid="netbox-run-detail-error">
			{run.error_message}
		</div>
	{/if}

	{#if planSummary}
		<div class="plan-summary" data-testid="netbox-executed-plan">
			<span class="plan-summary-label">Executed plan</span>
			<div class="plan-chips" aria-label="Executed plan summary by action">
				{#each NETBOX_PLAN_SUMMARY_CHIPS as chip (chip.key)}
					<span
						class="plan-chip"
						class:plan-chip-zero={planSummary[chip.key] === 0}
						data-testid="netbox-executed-plan-chip"
						data-netbox-action={chip.key}
					>
						<span class="chip-label">{chip.label}</span>
						<span class="chip-count">{planSummary[chip.key]}</span>
					</span>
				{/each}
			</div>
		</div>
	{/if}

	{#if loading}
		<div class="hint" role="status" data-testid="netbox-run-detail-loading">Loading run…</div>
	{:else if resultView === null}
		<p class="hint" data-testid="netbox-run-detail-pending">
			No per-entry results yet — the run has not executed.
		</p>
	{:else if 'raw' in resultView}
		<pre class="raw" data-testid="netbox-run-detail-raw">{resultView.raw}</pre>
	{:else}
		{#if resultView.error}
			<div class="detail-error" role="alert" data-testid="netbox-run-abort-error">
				Aborted: {resultView.error.message}
				{#if resultView.error.failed_chv_resource_ref}
					(at {resultView.error.failed_chv_resource_ref})
				{/if}
			</div>
		{/if}
		<ul class="outcomes" aria-label="Per-entry outcomes">
			{#each resultView.entries as outcome, i (i)}
				<li class="outcome" data-testid="netbox-run-outcome" data-netbox-outcome-status={outcome.status}>
					<span
						class="status-badge status-{outcome.status}"
						role="img"
						aria-label={`Entry status: ${statusLabels[outcome.status] ?? outcome.status}`}
					>
						{statusLabels[outcome.status] ?? outcome.status}
					</span>
					<span class="outcome-action">{NETBOX_ACTION_LABELS[outcome.action] ?? outcome.action}</span>
					<span class="outcome-ref">{outcome.chv_resource_ref}</span>
					{#if outcome.error}
						<span class="outcome-error">{outcome.error}</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>

<style>
	.detail {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.75rem;
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
		background: var(--chv-color-neutral-50, #f8fafc);
	}
	.detail-header { display: flex; align-items: baseline; gap: 0.5rem; flex-wrap: wrap; }
	.detail-title {
		margin: 0;
		font-size: var(--chv-text-sm);
		font-weight: 700;
		color: var(--chv-color-neutral-700);
	}
	.detail-meta { font-size: 12px; color: var(--chv-color-neutral-500); }
	.detail-facts { display: flex; gap: 1.25rem; flex-wrap: wrap; margin: 0; }
	.detail-facts dt { font-size: 11px; font-weight: 600; color: var(--chv-color-neutral-500); }
	.detail-facts dd { margin: 0; font-size: 12px; color: var(--chv-color-neutral-700); }
	.resolved-note { font-size: 10px; color: var(--chv-color-neutral-500); }
	.detail-error {
		padding: 0.5rem 0.75rem;
		border-radius: var(--radius-xs);
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		color: rgb(153, 27, 27);
		font-size: 12px;
	}
	.hint { margin: 0; font-size: 12px; color: var(--chv-color-neutral-500); }
	.plan-summary { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
	.plan-summary-label {
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-500);
	}
	.plan-chips { display: flex; flex-wrap: wrap; gap: 0.3rem; }
	.plan-chip {
		display: inline-flex;
		align-items: center;
		gap: 0.3rem;
		padding: 0.1rem 0.45rem;
		font-size: 11px;
		font-weight: 600;
		border-radius: var(--radius-xs);
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		color: var(--chv-color-neutral-600);
	}
	.plan-chip-zero { opacity: 0.7; }
	.chip-count { font-variant-numeric: tabular-nums; font-weight: 700; }
	.raw {
		margin: 0;
		padding: 0.5rem 0.75rem;
		background: var(--bg-surface);
		border: 1px dashed var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
		font-family: var(--chv-font-mono, ui-monospace, monospace);
		font-size: 11px;
		color: var(--chv-color-neutral-600);
		overflow-x: auto;
		white-space: pre-wrap;
	}
	.outcomes { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 0.3rem; }
	.outcome {
		display: flex;
		align-items: baseline;
		gap: 0.5rem;
		flex-wrap: wrap;
		padding: 0.3rem 0.5rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
	}
	.outcome-action { font-size: 11px; font-weight: 600; color: var(--chv-color-neutral-500); }
	.outcome-ref {
		font-family: var(--chv-font-mono, ui-monospace, monospace);
		font-size: 12px;
		color: var(--chv-color-neutral-700);
	}
	.outcome-error { flex-basis: 100%; font-size: 11px; color: rgb(153, 27, 27); }
	.status-badge {
		display: inline-block;
		padding: 0.1rem 0.5rem;
		border-radius: var(--radius-xs);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.02em;
		color: white;
		white-space: nowrap;
	}
	.status-succeeded { background: #15803d; }
	.status-failed { background: #b91c1c; }
	.status-skipped { background: #6b7280; }
	.status-not_attempted { background: #4b5563; }
</style>
