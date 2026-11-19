<script lang="ts">
	import type { Finding, ValidationSeverity } from '#lib/bff/architectures.ts';

	interface Props {
		finding: Finding;
		/** Emitted when the user clicks the resource_ref pill so a future
		 *  Phase 2 canvas can highlight the offending node. Phase 1 owners
		 *  can ignore this — it is wired through but not yet consumed. */
		onSelectResource?: (resourceRef: string) => void;
	}

	let { finding, onSelectResource }: Props = $props();

	const severityLabel: Record<ValidationSeverity, string> = {
		error: 'Error',
		warning: 'Warning',
		info: 'Info'
	};

	function handleSelect() {
		if (finding.resource_ref) {
			onSelectResource?.(finding.resource_ref);
		}
	}
</script>

<li class="finding finding-{finding.severity}" data-testid="finding-item">
	<div class="finding-head">
		<span
			class="severity-pill severity-{finding.severity}"
			aria-label={severityLabel[finding.severity]}
			data-testid="finding-severity"
		>
			{severityLabel[finding.severity]}
		</span>
		<code class="code" data-testid="finding-code">{finding.code}</code>
		{#if finding.blocking}
			<span class="blocking-pill" aria-label="Blocking">blocking</span>
		{/if}
	</div>

	<p class="message" data-testid="finding-message">{finding.message}</p>

	<div class="finding-foot">
		<code class="path" title={finding.path} data-testid="finding-path">{finding.path}</code>
		{#if finding.resource_ref}
			<button
				type="button"
				class="resource-pill"
				onclick={handleSelect}
				aria-label={`Select resource ${finding.resource_ref}`}
				data-testid="finding-resource-ref"
			>
				{finding.resource_ref}
			</button>
		{/if}
	</div>

	{#if finding.suggestion}
		<p class="suggestion" data-testid="finding-suggestion">
			<span class="suggestion-prefix">Try: </span>{finding.suggestion}
		</p>
	{/if}
</li>

<style>
	.finding {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		padding: 0.65rem 0.85rem;
		border: 1px solid var(--chv-color-neutral-200);
		border-left-width: 3px;
		border-radius: var(--radius-xs);
		background: var(--bg-surface);
		font-size: var(--chv-text-sm);
	}

	.finding-error {
		border-left-color: var(--color-danger, #b91c1c);
	}

	.finding-warning {
		border-left-color: #b45309; /* amber-700 — WCAG AA on white */
	}

	.finding-info {
		border-left-color: var(--color-primary);
	}

	.finding-head {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
	}

	.severity-pill {
		display: inline-block;
		padding: 0.1rem 0.45rem;
		font-size: 11px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		border-radius: var(--radius-xs);
		color: white;
	}

	.severity-error {
		background: #b91c1c; /* red-700 — 4.5:1 on white */
	}

	.severity-warning {
		background: #b45309;
	}

	.severity-info {
		background: #1d4ed8; /* blue-700 */
	}

	.blocking-pill {
		display: inline-block;
		padding: 0.05rem 0.4rem;
		font-size: 10px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: #7f1d1d;
		border: 1px solid #b91c1c;
		border-radius: var(--radius-xs);
		background: #fee2e2;
	}

	.code {
		font-family: var(--chv-font-mono, monospace);
		font-size: 12px;
		color: var(--chv-color-neutral-700);
	}

	.message {
		margin: 0;
		color: var(--chv-color-neutral-900);
		line-height: 1.4;
	}

	.finding-foot {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		flex-wrap: wrap;
	}

	.path {
		flex: 1 1 auto;
		min-width: 0;
		font-family: var(--chv-font-mono, monospace);
		font-size: 11px;
		color: var(--chv-color-neutral-500);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.resource-pill {
		flex: 0 0 auto;
		display: inline-block;
		padding: 0.1rem 0.45rem;
		font-family: var(--chv-font-mono, monospace);
		font-size: 11px;
		color: var(--chv-color-neutral-700);
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-300);
		border-radius: var(--radius-xs);
		cursor: pointer;
	}

	.resource-pill:hover {
		background: var(--chv-color-neutral-100);
	}

	.resource-pill:focus-visible {
		outline: 2px solid var(--color-primary);
		outline-offset: 2px;
	}

	.suggestion {
		margin: 0;
		font-size: 12px;
		color: var(--chv-color-neutral-600);
		line-height: 1.4;
	}

	.suggestion-prefix {
		font-weight: 600;
		color: var(--chv-color-neutral-700);
	}
</style>
