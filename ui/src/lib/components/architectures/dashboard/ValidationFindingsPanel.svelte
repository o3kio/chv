<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import FindingItem from './FindingItem.svelte';
	import type { Finding, ValidationResult, ValidationStatus } from '#lib/bff/architectures.ts';

	interface Props {
		/** The result to render. `null` means "not yet run" → show CTA. */
		result: ValidationResult | null;
		/** True while a validate request is in flight. */
		loading?: boolean;
		/** "Re-validate" / "Run validation" click handler. */
		onRevalidate: () => void;
		/** Forwarded to each FindingItem so a future Phase 2 canvas can light up the offending node. */
		onSelectResource?: (resourceRef: string) => void;
	}

	let { result, loading = false, onRevalidate, onSelectResource }: Props = $props();

	const SEVERITY_ORDER: Record<Finding['severity'], number> = {
		error: 0,
		warning: 1,
		info: 2
	};

	const sortedFindings = $derived.by<Finding[]>(() => {
		if (!result) return [];
		// Stable sort: severity (errors first) → code → path. The server may
		// already emit a stable order but we don't rely on that — the panel is
		// the canonical UI sort.
		return [...result.findings].sort((a, b) => {
			const sev = SEVERITY_ORDER[a.severity] - SEVERITY_ORDER[b.severity];
			if (sev !== 0) return sev;
			const code = a.code.localeCompare(b.code);
			if (code !== 0) return code;
			return a.path.localeCompare(b.path);
		});
	});

	const status = $derived<ValidationStatus | null>(result?.status ?? null);
	const summary = $derived(result?.summary ?? { errors: 0, warnings: 0, info: 0 });

	const statusLabel: Record<ValidationStatus, string> = {
		valid: 'Valid',
		warning: 'Warnings',
		invalid: 'Invalid'
	};
</script>

<section
	class="panel"
	aria-labelledby="validation-heading"
	data-testid="validation-findings-panel"
>
	<header class="panel-header">
		<div class="header-left">
			<h2 id="validation-heading" class="panel-title">Validation</h2>
			{#if status}
				<span
					class="status-pill status-{status}"
					aria-label={`Status: ${statusLabel[status]}`}
					data-testid="validation-status-pill"
				>
					{statusLabel[status]}
				</span>
				<span class="counts" aria-label="Finding counts">
					<span data-testid="count-errors">{summary.errors} errors</span>
					<span aria-hidden="true">·</span>
					<span data-testid="count-warnings">{summary.warnings} warnings</span>
					<span aria-hidden="true">·</span>
					<span data-testid="count-info">{summary.info} info</span>
				</span>
			{/if}
		</div>
		<div class="header-right">
			<Button
				variant="secondary"
				size="sm"
				loading={loading}
				onclick={onRevalidate}
				ariaLabel={result ? 'Re-validate architecture' : 'Run validation'}
			>
				{result ? 'Re-validate' : 'Run validation'}
			</Button>
		</div>
	</header>

	{#if !result && !loading}
		<div class="empty cta-empty" role="status">
			<p class="empty-title">No validation run yet.</p>
			<p class="empty-text">Click <strong>Run validation</strong> to check this topology against the schema and policy registry.</p>
		</div>
	{:else if result && sortedFindings.length === 0}
		<div class="empty" role="status" data-testid="validation-empty">
			<p class="empty-title">No findings — topology is valid.</p>
		</div>
	{:else}
		<ul class="findings-list" aria-label="Validation findings">
			{#each sortedFindings as finding (`${finding.code}|${finding.path}|${finding.message}`)}
				<FindingItem {finding} {onSelectResource} />
			{/each}
		</ul>
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
		background: #15803d; /* green-700 */
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
		color: var(--chv-color-neutral-600);
	}

	.findings-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}

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

	.empty-text {
		margin: 0.25rem 0 0 0;
		font-size: 12px;
		color: var(--chv-color-neutral-600);
	}
</style>
