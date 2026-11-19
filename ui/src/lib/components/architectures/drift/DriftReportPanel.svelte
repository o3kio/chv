<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import DriftStatusBadge from './DriftStatusBadge.svelte';
	import DriftSummaryChips from './DriftSummaryChips.svelte';
	import DriftFindingRow from './DriftFindingRow.svelte';
	import { architectureDriftStore } from '#lib/stores/architecture-drift-store.svelte.ts';
	import type { DriftFinding } from '#lib/bff/architectures.ts';

	interface Props {
		architectureId: string;
	}

	let { architectureId }: Props = $props();

	// `lastLoadedId` is intentionally NOT `$state` — making it reactive
	// would cause the effect to re-run after each internal write.
	let lastLoadedId: string | null = null;

	// Lazy-load on first activation. We key on architectureId so navigating
	// between architectures triggers a fresh fetch rather than showing the
	// previous report. When we detect a real id change, reset the store
	// FIRST so cross-arch navigation does not flash stale findings — then
	// kick off the fresh refresh.
	//
	// We deliberately do NOT register an unmount/cleanup `return` here.
	// Returning a cleanup that calls `reset()` would race with the
	// in-flight `refresh()` (cleanup runs synchronously, fetch resolves
	// asynchronously) and clobber the just-fetched report. Cross-arch
	// safety is provided by the store's own `lastArchitectureId` guard
	// (see architecture-drift-store.svelte.ts) and the explicit reset
	// branch below.
	$effect(() => {
		if (architectureId && architectureId !== lastLoadedId) {
			if (lastLoadedId !== null) {
				architectureDriftStore.reset();
			}
			lastLoadedId = architectureId;
			void architectureDriftStore.refresh(architectureId, false);
		}
	});

	const report = $derived(architectureDriftStore.report);
	const loading = $derived(architectureDriftStore.loading);
	const storeError = $derived(architectureDriftStore.error);

	// Group findings by code for the rendered list. Server already returns
	// them in a stable order; we preserve that order within each group.
	const grouped = $derived.by((): Map<DriftFinding['code'], DriftFinding[]> => {
		const out = new Map<DriftFinding['code'], DriftFinding[]>();
		for (const f of report?.findings ?? []) {
			const bucket = out.get(f.code) ?? [];
			bucket.push(f);
			out.set(f.code, bucket);
		}
		return out;
	});

	const totalFindings = $derived(report?.findings.length ?? 0);

	/**
	 * Tiny relative-time helper. The codebase has no shared formatter; a
	 * 5-line version mirrors FleetCheckPanel and avoids a date-fns dep.
	 */
	function relativeTime(iso: string): string {
		const then = new Date(iso).getTime();
		if (!Number.isFinite(then)) return iso;
		const diffMs = Date.now() - then;
		const diffSec = Math.max(0, Math.round(diffMs / 1000));
		if (diffSec < 5) return 'just now';
		if (diffSec < 60) return `${diffSec}s ago`;
		const diffMin = Math.round(diffSec / 60);
		if (diffMin < 60) return `${diffMin}m ago`;
		const diffHr = Math.round(diffMin / 60);
		if (diffHr < 24) return `${diffHr}h ago`;
		const diffDay = Math.round(diffHr / 24);
		return `${diffDay}d ago`;
	}

	const computedHint = $derived.by(() => {
		if (!report) return '';
		const rel = relativeTime(report.computed_at);
		return report.cache_hit ? `Computed ${rel} (cached)` : `Computed ${rel}`;
	});

	async function handleRefresh() {
		await architectureDriftStore.refresh(architectureId, true);
	}

	const refreshLabel = $derived(loading ? 'Refreshing…' : 'Refresh');
</script>

<section
	class="panel"
	aria-label="Configuration drift"
	data-testid="drift-report-panel"
>
	<header class="panel-header">
		<div class="header-left">
			<h2 class="panel-title">Configuration drift</h2>
			{#if report}
				<DriftStatusBadge status={report.status} />
				<span class="counts" aria-label="Drift finding count">
					<span data-testid="drift-total-count">
						{totalFindings} {totalFindings === 1 ? 'finding' : 'findings'}
					</span>
				</span>
				{#if computedHint}
					<span class="computed-hint" data-testid="drift-computed-at">{computedHint}</span>
				{/if}
			{:else if !loading}
				<DriftStatusBadge status="unknown" />
			{/if}
		</div>
		<div class="header-right">
			<Button
				variant="secondary"
				size="sm"
				loading={loading}
				onclick={handleRefresh}
				ariaLabel="Refresh drift report"
				data-testid="drift-refresh-button"
			>
				{refreshLabel}
			</Button>
		</div>
	</header>

	<!--
		Status banner is a polite live region so screen readers pick up the
		"Drifted" / "No drift" / "Check failed" announcement after a refresh.
	-->
	{#if report}
		{#if report.status === 'drifted'}
			<div
				class="banner banner-drifted"
				role="status"
				aria-live="polite"
				data-testid="drift-status-banner"
			>
				<strong>{totalFindings} drift {totalFindings === 1 ? 'finding' : 'findings'}.</strong>
				<span>The live fleet no longer matches the saved topology.</span>
			</div>
		{:else if report.status === 'check_failed'}
			<div
				class="banner banner-failed"
				role="alert"
				aria-live="assertive"
				data-testid="drift-failed-banner"
			>
				<strong>Drift check failed.</strong>
				<span>{report.error_message ?? 'Inventory snapshot or compute failed.'}</span>
			</div>
		{:else if report.status === 'no_drift'}
			<div
				class="banner banner-clean"
				role="status"
				aria-live="polite"
				data-testid="drift-status-banner"
			>
				<strong>No drift detected.</strong>
				<span>The live fleet matches the saved topology.</span>
			</div>
		{/if}

		{#if report.summary.total > 0 || report.status === 'drifted'}
			<DriftSummaryChips summary={report.summary} />
		{/if}

		{#if report.findings.length === 0 && report.status === 'no_drift'}
			<div class="empty" role="status" data-testid="drift-empty-state">
				<p class="empty-title">No drift to show.</p>
				<p class="empty-text">
					Click <strong>Refresh</strong> to recompute against a fresh inventory snapshot.
				</p>
			</div>
		{:else if report.findings.length > 0}
			<ol class="findings" aria-label="Drift findings grouped by type">
				{#each Array.from(grouped.entries()) as [code, items] (code)}
					{#each items as finding (`${code}-${finding.path}-${finding.resource_ref}`)}
						<DriftFindingRow {finding} />
					{/each}
				{/each}
			</ol>
		{/if}
	{:else if loading}
		<div class="loading" role="status" aria-live="polite">
			<p class="empty-title">Computing drift…</p>
		</div>
	{:else if storeError}
		<div class="banner banner-failed" role="alert" data-testid="drift-failed-banner">
			<strong>Could not load drift report.</strong>
			<span>{storeError}</span>
		</div>
	{:else}
		<div class="empty" role="status" data-testid="drift-empty-state">
			<p class="empty-title">No drift report yet.</p>
			<p class="empty-text">
				Click <strong>Refresh</strong> to capture a snapshot and compute the diff.
			</p>
		</div>
	{/if}
</section>

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		padding: 1rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-sm);
	}

	.panel-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 0.75rem;
		flex-wrap: wrap;
	}

	.header-left {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
	}

	.panel-title {
		font-size: var(--chv-text-sm);
		font-weight: 700;
		margin: 0;
		color: var(--chv-color-neutral-700);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.counts {
		font-size: 12px;
		color: var(--chv-color-neutral-600);
	}

	.computed-hint {
		font-size: 12px;
		color: var(--chv-color-neutral-500);
		padding: 0.1rem 0.5rem;
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
	}

	.banner {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		padding: 0.6rem 0.85rem;
		border-radius: var(--radius-xs);
		font-size: var(--chv-text-sm);
	}

	.banner-clean {
		background: rgba(16, 185, 129, 0.1);
		border: 1px solid rgba(16, 185, 129, 0.4);
		color: rgb(6, 95, 70);
	}

	.banner-drifted {
		background: rgba(180, 83, 9, 0.1);
		border: 1px solid rgba(180, 83, 9, 0.4);
		color: rgb(120, 53, 15);
	}

	.banner-failed {
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		color: rgb(153, 27, 27);
	}

	.empty,
	.loading {
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

	.empty-text {
		margin: 0.25rem 0 0 0;
		font-size: 12px;
		color: var(--chv-color-neutral-600);
	}

	.findings {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
</style>
