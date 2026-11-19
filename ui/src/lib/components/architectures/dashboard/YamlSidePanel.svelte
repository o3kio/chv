<script lang="ts">
	import Button from '#lib/components/primitives/Button.svelte';
	import { toast } from '#lib/stores/toast.svelte.ts';

	interface Props {
		/** Pre-rendered YAML, or null when none has been generated yet. */
		yaml: string | null;
		loading?: boolean;
		/** Reason text when yaml is null and loading is false (e.g. "graph is empty"). */
		emptyReason?: string;
		/** Click handler for the "Generate" / "Refresh" button. */
		onGenerate: () => void;
	}

	let { yaml, loading = false, emptyReason, onGenerate }: Props = $props();

	async function handleCopy() {
		if (!yaml) return;
		try {
			await navigator.clipboard.writeText(yaml);
			toast.success('YAML copied to clipboard');
		} catch {
			// Clipboard API can fail in non-secure contexts or older browsers.
			// We surface a concrete error rather than silently dropping.
			toast.error('Could not copy YAML — clipboard unavailable');
		}
	}
</script>

<section class="panel" aria-labelledby="yaml-heading" data-testid="yaml-side-panel">
	<header class="panel-header">
		<h2 id="yaml-heading" class="panel-title">YAML</h2>
		<div class="actions">
			<Button
				variant="secondary"
				size="sm"
				onclick={onGenerate}
				loading={loading}
				ariaLabel={yaml ? 'Refresh YAML' : 'Generate YAML'}
			>
				{yaml ? 'Refresh' : 'Generate'}
			</Button>
			<Button
				variant="ghost"
				size="sm"
				disabled={!yaml}
				onclick={handleCopy}
				ariaLabel="Copy YAML to clipboard"
				title="Copy YAML to clipboard"
			>
				Copy
			</Button>
		</div>
	</header>

	{#if yaml}
		<pre class="yaml-block" data-testid="yaml-content"><code>{yaml}</code></pre>
	{:else if loading}
		<div class="empty" role="status" aria-live="polite">
			<p class="empty-title">Generating YAML…</p>
		</div>
	{:else}
		<div class="empty" role="status" data-testid="yaml-empty">
			<p class="empty-title">No YAML yet.</p>
			<p class="empty-text">
				{emptyReason ?? 'Design the topology first, then click Generate to render its canonical YAML.'}
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
		gap: 0.5rem;
	}

	.panel-title {
		font-size: var(--chv-text-sm);
		font-weight: 700;
		margin: 0;
		color: var(--chv-color-neutral-700);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.actions {
		display: flex;
		gap: 0.4rem;
	}

	.yaml-block {
		max-height: 480px;
		overflow: auto;
		padding: 0.75rem;
		margin: 0;
		font-family: var(--chv-font-mono, ui-monospace, monospace);
		font-size: 12px;
		line-height: 1.5;
		color: var(--chv-color-neutral-900);
		background: var(--chv-color-neutral-50, #f8fafc);
		border: 1px solid var(--chv-color-neutral-200);
		border-radius: var(--radius-xs);
		white-space: pre;
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
		margin: 0.25rem auto 0;
		max-width: 480px;
		font-size: 12px;
		color: var(--chv-color-neutral-600);
	}
</style>
