<script lang="ts">
	import type { HTMLInputAttributes } from 'svelte/elements';
	import VisuallyHidden from '../shared/VisuallyHidden.svelte';

	interface Props extends HTMLInputAttributes {
		value?: string | number;
		label?: string;
		error?: string;
		hint?: string;
		required?: boolean;
	}

	let {
		value = $bindable(''),
		label,
		error,
		hint,
		required = false,
		type = 'text',
		placeholder,
		disabled,
		id,
		...rest
	}: Props = $props();

	let generatedId = $state(`input-${Math.random().toString(36).slice(2, 11)}`);
	let inputId = $derived(id ?? generatedId);
	let errorId = $derived(error ? `${inputId}-error` : undefined);
	let hintId = $derived(hint ? `${inputId}-hint` : undefined);
	let describedBy = $derived([errorId, hintId].filter(Boolean).join(' ') || undefined);

	const baseClasses =
		'w-full py-2.5 px-3.5 text-sm rounded-xs bg-(--bg-surface) text-(--chv-color-neutral-900) transition-all duration-150 ease-in-out focus:outline-hidden disabled:bg-(--chv-color-neutral-100) disabled:text-(--chv-color-neutral-400) disabled:cursor-not-allowed placeholder:text-(--chv-color-neutral-400)';

	const errorClasses =
		'border-danger shadow-[0_0_0_3px_var(--color-danger-glow)] hover:border-danger focus:border-danger focus:shadow-[0_0_0_3px_var(--color-danger-glow)]';

	const normalClasses =
		'border-(--chv-color-neutral-300) hover:border-(--chv-color-neutral-400) focus:border-primary focus:shadow-[0_0_0_3px_var(--color-primary-glow)]';
</script>

<div class="flex flex-col gap-2">
	{#if label}
		<label for={inputId} class="text-sm font-medium text-(--chv-color-neutral-700) flex items-center gap-1">
			{label}
			{#if required}
				<span class="text-danger" aria-hidden="true">*</span>
				<VisuallyHidden>(required)</VisuallyHidden>
			{/if}
		</label>
	{/if}
	<input
		bind:value
		{type}
		{placeholder}
		{disabled}
		id={inputId}
		class="{baseClasses} {error ? errorClasses : normalClasses}"
		aria-invalid={error ? 'true' : 'false'}
		aria-describedby={describedBy}
		aria-required={required}
		{...rest}
	/>
	{#if error}
		<span id={errorId} class="text-xs text-danger" role="alert" aria-live="assertive">
			{error}
		</span>
	{:else if hint}
		<span id={hintId} class="text-xs text-(--chv-color-neutral-500)">
			{hint}
		</span>
	{/if}
</div>
