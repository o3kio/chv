<script lang="ts">
	import type { Snippet } from 'svelte';
	import VisuallyHidden from './VisuallyHidden.svelte';

	interface Props {
		label: string;
		error?: string;
		helper?: string;
		required?: boolean;
		labelFor?: string;
		children: Snippet;
	}

	let {
		label,
		error,
		helper,
		required = false,
		labelFor,
		children
	}: Props = $props();

	let fieldId = $derived(labelFor || `field-${Math.random().toString(36).slice(2, 9)}`);
	let helperId = $derived(helper ? `${fieldId}-helper` : undefined);
	let errorId = $derived(error ? `${fieldId}-error` : undefined);
</script>

<div class="form-field">
	<!-- Label -->
	<label
		for={fieldId}
		class="form-label"
	>
		{label}
		{#if required}
			<span class="required-indicator" aria-hidden="true">*</span>
			<VisuallyHidden>(required)</VisuallyHidden>
		{/if}
	</label>

	<!-- Input/Select with aria-describedby for helper/error -->
	{#snippet fieldSnippet()}
		{@render children()}
	{/snippet}
	
	{@render children()}

	<!-- Helper Text -->
	{#if helper && !error}
		<p id={helperId} class="helper-text">
			{helper}
		</p>
	{/if}

	<!-- Error Message -->
	{#if error}
		<div id={errorId} class="error-container" role="alert" aria-live="assertive">
			<svg
				xmlns="http://www.w3.org/2000/svg"
				width="14"
				height="14"
				viewBox="0 0 24 24"
				fill="none"
				stroke="currentColor"
				stroke-width="2"
				stroke-linecap="round"
				stroke-linejoin="round"
				class="error-icon"
				aria-hidden="true"
			>
				<circle cx="12" cy="12" r="10" />
				<line x1="12" x2="12" y1="8" y2="12" />
				<line x1="12" x2="12.01" y1="16" y2="16" />
			</svg>
			<p class="error-text">{error}</p>
		</div>
	{/if}
</div>

<style>
	.form-field {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.form-label {
		font-size: var(--chv-text-sm);
		font-weight: 500;
		color: var(--chv-color-neutral-700);
		display: flex;
		align-items: center;
		gap: var(--space-1);
	}

	.required-indicator {
		color: var(--color-danger);
	}

	.helper-text {
		font-size: var(--chv-text-xs);
		color: var(--chv-color-neutral-500);
		margin: 0;
	}

	.error-container {
		display: flex;
		align-items: flex-start;
		gap: var(--space-1);
		margin: 0;
	}

	.error-icon {
		color: var(--color-danger);
		flex-shrink: 0;
		margin-top: 1px;
	}

	.error-text {
		font-size: var(--chv-text-xs);
		color: var(--color-danger);
		margin: 0;
	}
</style>
