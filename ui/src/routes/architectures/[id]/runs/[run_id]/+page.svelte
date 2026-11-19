<script lang="ts">
	import { goto } from '$app/navigation';
	import { onDestroy } from 'svelte';
	import Button from '#lib/components/primitives/Button.svelte';
	import RunStatusBadge from '#lib/components/architectures/runs/RunStatusBadge.svelte';
	import OperationProgressRow from '#lib/components/architectures/runs/OperationProgressRow.svelte';
	import type { OperationProgress } from '#lib/components/architectures/runs/types.ts';
	import { architectureRunsStore } from '#lib/stores/architecture-runs-store.svelte.ts';
	import { isTerminalRunStatus, type ApplyRunDetail } from '#lib/bff/architectures.ts';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();
	const model = $derived(data.model);

	// Seed the store with the loader's run so the badge paints immediately.
	$effect(() => {
		if (model.state === 'ready') {
			architectureRunsStore.state.currentRun = model.run;
		}
	});

	// Live run is the polled copy from the store (falls back to loader copy).
	const liveRun = $derived<ApplyRunDetail | null>(
		architectureRunsStore.state.currentRun ??
			(model.state === 'ready' ? model.run : null)
	);

	const heading = $derived(
		model.state === 'ready'
			? `Run ${model.run.id.slice(0, 12)}`
			: 'Run'
	);

	// Start polling on mount when the run is non-terminal; stop on destroy.
	$effect(() => {
		if (model.state !== 'ready') return;
		if (isTerminalRunStatus(model.run.status)) return;
		architectureRunsStore.pollRun(model.architecture.id, model.run.id);
	});

	onDestroy(() => {
		architectureRunsStore.stopPolling();
	});

	function fmtTime(iso: string | null): string {
		if (!iso) return '—';
		try {
			return new Date(iso).toLocaleString();
		} catch {
			return iso;
		}
	}

	function fmtDuration(start: string | null, end: string | null): string {
		if (!start) return '—';
		const s = new Date(start).getTime();
		const e = end ? new Date(end).getTime() : Date.now();
		const ms = Math.max(0, e - s);
		const sec = Math.round(ms / 1000);
		if (sec < 60) return `${sec}s`;
		const m = Math.floor(sec / 60);
		const r = sec % 60;
		return `${m}m ${r}s`;
	}

	/**
	 * Best-effort parse of `result_json`. The orchestrator writes a
	 * `{ operations: OperationProgress[] }` payload on terminal transitions;
	 * we tolerate missing/malformed data so a freshly-queued run still
	 * renders something useful.
	 */
	const operations = $derived.by<OperationProgress[]>(() => {
		if (!liveRun?.result_json) return [];
		try {
			const parsed = JSON.parse(liveRun.result_json) as {
				operations?: OperationProgress[];
			};
			return Array.isArray(parsed.operations) ? parsed.operations : [];
		} catch {
			return [];
		}
	});

	const showFallback = $derived(operations.length === 0 && !!liveRun?.task_id);
</script>

<svelte:head>
	<title>{heading} · CellHV</title>
</svelte:head>

<div class="page" data-testid="architecture-run-page">
	{#if model.state === 'error'}
		<div class="error-banner" role="alert" data-testid="run-load-error">
			<strong>Could not load run {model.runId}.</strong>
			<span>{model.errorMessage}</span>
			<div>
				<Button
					variant="secondary"
					size="sm"
					onclick={() => goto(`/architectures/${model.architectureId}/runs`)}
				>
					Back to runs
				</Button>
			</div>
		</div>
	{:else if liveRun}
		<header class="page-header">
			<div>
				<button
					type="button"
					class="back-link"
					onclick={() => goto(`/architectures/${model.architecture.id}/runs`)}
					aria-label="Back to runs list"
				>
					← Runs
				</button>
				<h1 class="page-title">Run {liveRun.id}</h1>
				<p class="page-subtitle">
					{model.architecture.display_name ?? model.architecture.name}
				</p>
			</div>
			<div class="header-status">
				<RunStatusBadge status={liveRun.status} />
				{#if architectureRunsStore.state.polling}
					<span class="polling" data-testid="run-polling-indicator" aria-live="polite">
						Polling…
					</span>
				{/if}
			</div>
		</header>

		<section class="facts" aria-label="Run facts" data-testid="run-facts">
			<dl class="dl">
				<div><dt>Started</dt><dd data-testid="run-started-at">{fmtTime(liveRun.started_at ?? liveRun.created_at)}</dd></div>
				<div><dt>Finished</dt><dd data-testid="run-finished-at">{fmtTime(liveRun.finished_at)}</dd></div>
				<div><dt>Duration</dt><dd data-testid="run-duration">{fmtDuration(liveRun.started_at, liveRun.finished_at)}</dd></div>
				<div><dt>Plan</dt><dd data-testid="run-plan-id">{liveRun.plan_id ?? '—'}</dd></div>
				<div><dt>Requested by</dt><dd data-testid="run-requested-by">{liveRun.requested_by ?? '—'}</dd></div>
				<div><dt>Task</dt>
					<dd data-testid="run-task-id">
						{#if liveRun.task_id}
							<a class="link" href={`/operations/${liveRun.task_id}`}>{liveRun.task_id}</a>
						{:else}
							—
						{/if}
					</dd>
				</div>
			</dl>
		</section>

		{#if liveRun.status === 'failed' || liveRun.status === 'partially_failed'}
			<div class="banner banner-error" role="alert" data-testid="run-error-banner">
				<strong>
					{liveRun.status === 'failed' ? 'Run failed.' : 'Run partially failed.'}
				</strong>
				{#if liveRun.error_message}
					<span data-testid="run-error-message">{liveRun.error_message}</span>
				{/if}
			</div>
		{:else if liveRun.status === 'succeeded'}
			<div class="banner banner-ok" role="status" data-testid="run-success-banner">
				<strong>All operations succeeded.</strong>
			</div>
		{:else if liveRun.status === 'cancelled'}
			<div class="banner banner-warn" role="status" data-testid="run-cancelled-banner">
				<strong>Run was cancelled.</strong>
			</div>
		{/if}

		<section class="ops-section" aria-label="Operation progress">
			<h2 class="ops-title">Operations</h2>
			{#if operations.length > 0}
				<ul class="ops-list">
					{#each operations as op, i (op.operation_id ?? i)}
						<OperationProgressRow {op} />
					{/each}
				</ul>
			{:else if showFallback}
				<p class="ops-fallback" data-testid="run-operations-fallback">
					Operations are tracked individually; visit
					<a class="link" href={`/operations/${liveRun.task_id}`}>
						/operations/{liveRun.task_id}
					</a>
					to stream progress.
				</p>
			{:else}
				<p class="ops-fallback" data-testid="run-operations-empty">
					No operation progress recorded yet.
				</p>
			{/if}
		</section>
	{/if}
</div>

<style>
	.page { display: flex; flex-direction: column; gap: 1rem; }
	.page-header {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 1rem;
		flex-wrap: wrap;
	}
	.back-link {
		background: none;
		border: none;
		padding: 0;
		font-size: var(--chv-text-xs);
		color: var(--chv-color-neutral-500);
		cursor: pointer;
		align-self: flex-start;
	}
	.back-link:hover, .back-link:focus-visible {
		color: var(--color-primary);
		outline: none;
	}
	.page-title {
		font-size: var(--chv-text-lg);
		font-weight: 700;
		margin: 0;
		font-family: var(--chv-font-mono, monospace);
	}
	.page-subtitle { margin: 0; font-size: var(--chv-text-sm); color: var(--chv-color-neutral-600); }
	.header-status { display: flex; gap: 0.5rem; align-items: center; }
	.polling {
		font-size: 11px;
		color: var(--chv-color-neutral-500);
		font-style: italic;
	}
	.facts {
		padding: 0.75rem 1rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
	}
	.dl {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 0.6rem 1rem;
		margin: 0;
	}
	.dl > div { display: flex; flex-direction: column; gap: 0.1rem; }
	.dl dt {
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-500);
	}
	.dl dd { margin: 0; font-size: var(--chv-text-sm); color: var(--chv-color-neutral-900); }
	.banner {
		display: flex;
		flex-wrap: wrap;
		gap: 0.4rem;
		padding: 0.6rem 0.85rem;
		border-radius: var(--radius-xs);
		font-size: var(--chv-text-sm);
	}
	.banner-error {
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		color: rgb(153, 27, 27);
	}
	.banner-ok {
		background: rgba(21, 128, 61, 0.08);
		border: 1px solid rgba(21, 128, 61, 0.4);
		color: rgb(20, 83, 45);
	}
	.banner-warn {
		background: rgba(245, 158, 11, 0.08);
		border: 1px solid rgba(245, 158, 11, 0.4);
		color: rgb(120, 53, 15);
	}
	.ops-section { display: flex; flex-direction: column; gap: 0.5rem; }
	.ops-title {
		margin: 0;
		font-size: var(--chv-text-sm);
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--chv-color-neutral-700);
	}
	.ops-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}
	.ops-fallback { margin: 0; font-size: var(--chv-text-sm); color: var(--chv-color-neutral-600); }
	.link {
		color: var(--color-primary);
		text-decoration: none;
	}
	.link:hover, .link:focus-visible { text-decoration: underline; }
	.error-banner {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.75rem 1rem;
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		border-radius: var(--radius-xs);
		color: rgb(153, 27, 27);
		font-size: var(--chv-text-sm);
	}
</style>
