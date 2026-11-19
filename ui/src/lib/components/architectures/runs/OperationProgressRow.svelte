<script lang="ts">
	import type { OperationProgress } from './types';

	interface Props {
		op: OperationProgress;
	}

	let { op }: Props = $props();

	const statusLabel: Record<OperationProgress['status'], string> = {
		pending: 'Pending',
		running: 'Running',
		succeeded: 'Succeeded',
		failed: 'Failed',
		cancelled: 'Cancelled'
	};
</script>

<li class="row" data-testid="operation-progress-row" data-status={op.status}>
	<div class="head">
		<span class="ref" data-testid="operation-progress-ref">{op.resource_ref}</span>
		<span class="action" data-testid="operation-progress-action">{op.action}</span>
		<span class="status status-{op.status}" data-testid="operation-progress-status">
			{statusLabel[op.status]}
		</span>
	</div>
	{#if op.status === 'failed' && op.error_message}
		<p class="err" role="alert" data-testid="operation-progress-error">{op.error_message}</p>
	{/if}
	{#if op.operation_id}
		<a
			class="link"
			href="/operations/{op.operation_id}"
			data-testid="operation-progress-link"
			aria-label={`Open operation ${op.operation_id}`}
		>
			View operation →
		</a>
	{/if}
</li>

<style>
	.row {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 0.55rem 0.85rem;
		background: var(--bg-surface);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
	}
	.head { display: flex; gap: 0.6rem; align-items: center; flex-wrap: wrap; }
	.ref {
		font-family: var(--chv-font-mono, monospace);
		font-size: var(--chv-text-sm);
		color: var(--chv-color-neutral-900);
		font-weight: 600;
	}
	.action {
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-size: 11px;
		color: var(--chv-color-neutral-600);
	}
	.status {
		display: inline-block;
		padding: 0.05rem 0.45rem;
		border-radius: var(--radius-xs);
		font-size: 11px;
		font-weight: 600;
		color: white;
	}
	.status-pending { background: #6b7280; }
	.status-running { background: #1d4ed8; }
	.status-succeeded { background: #15803d; }
	.status-failed { background: #b91c1c; }
	.status-cancelled { background: #4b5563; }
	.err {
		margin: 0;
		font-size: 12px;
		color: rgb(153, 27, 27);
	}
	.link {
		font-size: 12px;
		color: var(--color-primary);
		text-decoration: none;
	}
	.link:hover, .link:focus-visible { text-decoration: underline; }
</style>
