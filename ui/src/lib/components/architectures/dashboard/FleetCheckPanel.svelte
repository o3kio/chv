<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import ValidationFindingsPanel from './ValidationFindingsPanel.svelte';
	import type {
		FleetCheckResult,
		ValidationResult,
		ValidationStatus
	} from '#lib/bff/architectures.ts';

	interface Props {
		/** The latest fleet-check result. `null` means "not yet run". */
		result: FleetCheckResult | null;
		/** True while a check-fleet request is in flight. */
		loading?: boolean;
		/** Click handler for the "Refresh inventory" button. */
		onRefresh: () => void;
	}

	let { result, loading = false, onRefresh }: Props = $props();

	const errorCount = $derived(
		result ? result.findings.filter((f) => f.severity === 'error').length : 0
	);
	const warningCount = $derived(
		result ? result.findings.filter((f) => f.severity === 'warning').length : 0
	);
	const infoCount = $derived(
		result ? result.findings.filter((f) => f.severity === 'info').length : 0
	);

	const blocked = $derived(errorCount > 0);

	/**
	 * Adapt FleetCheckResult into a ValidationResult so we can reuse the
	 * Phase-1 ValidationFindingsPanel for finding rendering. The fleet panel
	 * owns its own toolbar (refresh button, status pill, last-checked pill);
	 * we suppress the embedded panel's revalidate button by passing a no-op
	 * onRevalidate and hide it visually via aria/css below.
	 */
	const adaptedResult = $derived<ValidationResult | null>(
		result
			? {
					status: result.status,
					summary: { errors: errorCount, warnings: warningCount, info: infoCount },
					findings: result.findings
				}
			: null
	);

	const statusLabel: Record<ValidationStatus, string> = {
		valid: 'Valid',
		warning: 'Warnings',
		invalid: 'Invalid'
	};

	/**
	 * Tiny relative-time helper. The codebase has no shared formatter; a
	 * 5-line version suffices here (per the spec's allowance) and avoids
	 * pulling in date-fns just for one pill.
	 */
	function relativeTime(iso: string): string {
		const then = new Date(iso).getTime();
		if (!Number.isFinite(then)) return iso;
		const diffMs = Date.now() - then;
		const diffSec = Math.max(0, Math.round(diffMs / 1000));
		if (diffSec < 60) return `${diffSec}s ago`;
		const diffMin = Math.round(diffSec / 60);
		if (diffMin < 60) return `${diffMin}m ago`;
		const diffHr = Math.round(diffMin / 60);
		if (diffHr < 24) return `${diffHr}h ago`;
		const diffDay = Math.round(diffHr / 24);
		return `${diffDay}d ago`;
	}

	const refreshLabel = $derived(loading ? 'Capturing inventory…' : 'Refresh inventory');
</script>

<section
	class="panel"
	aria-label="Fleet check findings"
	data-testid="fleet-check-panel"
>
	<header class="panel-header">
		<div class="header-left">
			<h2 class="panel-title">Fleet check</h2>
			{#if result}
				<span
					class="status-pill status-{result.status}"
					aria-label={`Fleet status: ${statusLabel[result.status]}`}
					data-testid="fleet-status-pill"
				>
					{statusLabel[result.status]}
				</span>
				<span class="counts" aria-label="Fleet finding counts">
					<span data-testid="fleet-count-errors">{errorCount} errors</span>
					<span aria-hidden="true">·</span>
					<span data-testid="fleet-count-warnings">{warningCount} warnings</span>
					<span aria-hidden="true">·</span>
					<span data-testid="fleet-count-info">{infoCount} info</span>
				</span>
				<span class="checked-at" data-testid="fleet-checked-at">
					Last checked: {relativeTime(result.checked_at)}
				</span>
			{/if}
		</div>
		<div class="header-right">
			<Button
				variant="secondary"
				size="sm"
				loading={loading}
				onclick={onRefresh}
				ariaLabel={refreshLabel}
				data-testid="fleet-refresh-button"
			>
				{refreshLabel}
			</Button>
		</div>
	</header>

	{#if blocked}
		<div
			class="deploy-blocked"
			role="alert"
			data-testid="fleet-deploy-blocked-banner"
		>
			<strong>Deploy blocked by {errorCount} fleet {errorCount === 1 ? 'error' : 'errors'}.</strong>
			<span>Resolve before applying.</span>
		</div>
	{/if}

	{#if result === null && !loading}
		<div class="empty" role="status" data-testid="fleet-empty">
			<p class="empty-title">No fleet check has been run yet.</p>
			<p class="empty-text">
				Click <strong>Refresh inventory</strong> to capture a fresh snapshot.
			</p>
		</div>
	{:else}
		<!--
			Reuse ValidationFindingsPanel for the finding list. We pass a no-op
			onRevalidate (the fleet panel owns its own refresh button up top)
			and hide the inner panel's header via the wrapper class to avoid
			duplicate "Validation" titles and pills.
		-->
		<div class="findings-host">
			<ValidationFindingsPanel
				result={adaptedResult}
				loading={loading}
				onRevalidate={onRefresh}
			/>
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
		border: 1px solid var(--color-neutral-200);
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
		font-size: var(--text-sm);
		font-weight: 700;
		margin: 0;
		color: var(--color-neutral-700);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.status-pill {
		display: inline-block;
		padding: 0.1rem 0.5rem;
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		border-radius: var(--radius-xs);
		color: white;
	}

	.status-valid {
		background: #15803d;
	}
	.status-warning {
		background: #b45309;
	}
	.status-invalid {
		background: #b91c1c;
	}

	.counts {
		display: inline-flex;
		gap: 0.4rem;
		font-size: 12px;
		color: var(--color-neutral-600);
	}

	.checked-at {
		font-size: 12px;
		color: var(--color-neutral-500);
		padding: 0.1rem 0.5rem;
		background: var(--color-neutral-50, #f8fafc);
		border: 1px solid var(--color-neutral-200);
		border-radius: var(--radius-xs);
	}

	.deploy-blocked {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		padding: 0.6rem 0.85rem;
		background: rgba(220, 38, 38, 0.08);
		border: 1px solid rgba(220, 38, 38, 0.4);
		border-radius: var(--radius-xs);
		color: rgb(153, 27, 27);
		font-size: var(--text-sm);
	}

	.empty {
		padding: 1rem;
		border: 1px dashed var(--color-neutral-300);
		border-radius: var(--radius-xs);
		text-align: center;
		background: var(--color-neutral-50, #f8fafc);
	}

	.empty-title {
		margin: 0;
		font-size: var(--text-sm);
		font-weight: 600;
		color: var(--color-neutral-700);
	}

	.empty-text {
		margin: 0.25rem 0 0 0;
		font-size: 12px;
		color: var(--color-neutral-600);
	}

	/*
	 * The reused ValidationFindingsPanel renders its own header with a
	 * "Validation" title, status pill, and re-validate button. The fleet
	 * panel already shows fleet-specific equivalents up top, so hide the
	 * embedded header to avoid duplication. We keep the panel mounted (its
	 * findings list, sorting, and a11y semantics are what we want to reuse).
	 */
	.findings-host :global(.panel) {
		padding: 0;
		border: none;
		background: transparent;
	}
	.findings-host :global(.panel-header) {
		display: none;
	}
</style>
