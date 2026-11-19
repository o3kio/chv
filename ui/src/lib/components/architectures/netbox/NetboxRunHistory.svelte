<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import NetboxRunDetail from './NetboxRunDetail.svelte';
	import type { NetboxRunDetail as NetboxRunDetailType, NetboxRunSummary } from '#lib/bff/architectures.ts';

	/**
	 * NetBox projection run history: one row per run (trigger, status,
	 * mode, summary counts, error, timestamps). Selecting a row renders
	 * the full run view (per-entry outcomes) via NetboxRunDetail. Failed
	 * rows expose a retry button; the 409
	 * `PROJECTION_RUN_NOT_RETRYABLE` / `NETBOX_RUN_ACTIVE` refusals are
	 * surfaced by the parent panel's inline retry banner (the store
	 * rethrows them after toasting, capturing code+message in
	 * `retryError`).
	 */

	interface Props {
		runs: NetboxRunSummary[];
		loading: boolean;
		currentRun: NetboxRunDetailType | null;
		runLoading: boolean;
		retrying: boolean;
		onSelectRun: (runId: string) => void;
		onRetry: (runId: string) => void;
	}

	let { runs, loading, currentRun, runLoading, retrying, onSelectRun, onRetry }: Props = $props();

	const statusLabels: Record<string, string> = {
		queued: 'Queued',
		running: 'Running',
		succeeded: 'Succeeded',
		failed: 'Failed'
	};

	const triggerLabels: Record<string, string> = {
		manual: 'Manual',
		post_apply: 'Post-apply'
	};

	function summaryText(run: NetboxRunSummary): string {
		if (!run.summary) return '—';
		const parts: string[] = [];
		if (run.summary.create > 0) parts.push(`${run.summary.create} create`);
		if (run.summary.update > 0) parts.push(`${run.summary.update} update`);
		if (run.summary.no_op > 0) parts.push(`${run.summary.no_op} unchanged`);
		if (run.summary.conflict > 0) parts.push(`${run.summary.conflict} conflict`);
		if (run.summary.stale > 0) parts.push(`${run.summary.stale} stale`);
		return parts.length > 0 ? parts.join(', ') : 'no entries';
	}

	function formatTime(iso: string | null): string {
		if (!iso) return '—';
		return new Date(iso).toLocaleString();
	}
</script>

<div class="history" data-testid="netbox-run-history">
	{#if loading}
		<div class="hint" role="status" data-testid="netbox-runs-loading">Loading run history…</div>
	{:else if runs.length === 0}
		<div class="empty" role="status" data-testid="netbox-runs-empty">
			<p class="empty-title">No projection runs yet.</p>
			<p class="empty-text">Exports appear here with their per-entry outcomes.</p>
		</div>
	{:else}
		<ul class="rows" aria-label="NetBox projection runs">
			{#each runs as run (run.id)}
				<li
					class="row"
					class:row-selected={currentRun?.id === run.id}
					data-testid="netbox-run-row"
					data-netbox-run-status={run.status}
				>
					<button
						type="button"
						class="row-main"
						onclick={() => onSelectRun(run.id)}
						aria-expanded={currentRun?.id === run.id}
						aria-label={`Show details for run ${run.id}`}
						data-testid="netbox-run-row-select"
					>
						<span
							class="status-badge status-{run.status}"
							role="img"
							aria-label={`Run status: ${statusLabels[run.status] ?? run.status}`}
							data-testid="netbox-run-status-badge"
						>
							{statusLabels[run.status] ?? run.status}
						</span>
						<span class="row-trigger">{triggerLabels[run.trigger] ?? run.trigger}</span>
						<span class="row-mode">{run.mode === 'dry_run' ? 'Dry run' : 'Export'}</span>
						<span class="row-summary" data-testid="netbox-run-summary">{summaryText(run)}</span>
						<span class="row-time" title={`Created ${run.created_at}`}>
							{formatTime(run.created_at)}
						</span>
					</button>
					{#if run.status === 'failed'}
						<Button
							variant="secondary"
							size="sm"
							onclick={() => onRetry(run.id)}
							loading={retrying}
							data-testid="netbox-run-retry-button"
							ariaLabel={`Retry run ${run.id}`}
						>
							Retry
						</Button>
					{/if}
					{#if run.error_message}
						<span class="row-error" role="alert" data-testid="netbox-run-error">
							{run.error_message}
						</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}

	{#if currentRun}
		<NetboxRunDetail run={currentRun} loading={runLoading} />
	{/if}
</div>

<style>
	.history { display: flex; flex-direction: column; gap: 0.6rem; }
	.hint { font-size: 12px; color: var(--chv-color-neutral-500); }
	.empty {
		padding: 1rem;
		border: 1px dashed var(--chv-color-neutral-300);
		border-radius: var(--radius-xs);
		text-align: center;
		background: var(--chv-color-neutral-50, #f8fafc);
	}
	.empty-title {
		margin: 0;
		font-size: var(--chv-text-sm);
		font-weight: 600;
		color: var(--chv-color-neutral-700);
	}
	.empty-text { margin: 0.25rem 0 0 0; font-size: 12px; color: var(--chv-color-neutral-600); }
	.rows { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 0.4rem; }
	.row {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
		padding: 0.4rem 0.6rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
	}
	.row-selected { border-color: var(--color-primary); }
	.row-main {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		flex: 1;
		flex-wrap: wrap;
		min-width: 0;
		background: none;
		border: none;
		padding: 0;
		cursor: pointer;
		text-align: left;
		font: inherit;
		color: inherit;
	}
	.row-main:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: 2px;
		border-radius: var(--radius-xs);
	}
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
	.status-queued { background: #6b7280; }
	.status-running { background: #1d4ed8; }
	.status-succeeded { background: #15803d; }
	.status-failed { background: #b91c1c; }
	.row-trigger, .row-mode { font-size: 11px; font-weight: 600; color: var(--chv-color-neutral-500); }
	.row-summary { font-size: 12px; color: var(--chv-color-neutral-700); }
	.row-time { font-size: 11px; color: var(--chv-color-neutral-400); margin-left: auto; }
	.row-error { flex-basis: 100%; font-size: 12px; color: rgb(153, 27, 27); }
</style>
