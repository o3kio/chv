<script lang="ts">
	import type { HTMLSelectAttributes } from 'svelte/elements';

	interface SelectOption {
		value: string;
		label: string;
		disabled?: boolean;
		title?: string;
	}

	interface Props extends HTMLSelectAttributes {
		value?: string;
		options: SelectOption[];
		placeholder?: string;
		error?: string;
	}

	let {
		value = $bindable(''),
		options,
		placeholder,
		error,
		disabled,
		id,
		class: className = '',
		...rest
	}: Props = $props();

	let selectRef = $state<HTMLSelectElement | null>(null);
	let isFocused = $state(false);

	export function focus() {
		selectRef?.focus();
	}

	const baseClasses =
		'h-9 w-full appearance-none rounded-sm border bg-(--bg-surface) px-3 py-2 pr-10 text-sm font-sans transition-colors duration-150';

	const stateClasses = $derived(() => {
		if (disabled) {
			return 'border-(--border-subtle) bg-(--bg-surface-muted) text-muted cursor-not-allowed';
		}
		if (error) {
			return 'border-danger bg-(--bg-surface) text-ink focus:border-danger focus:outline-hidden focus:ring-2 focus:ring-danger/20';
		}
		if (isFocused) {
			return 'border-primary bg-(--bg-surface) text-ink focus:outline-hidden focus:ring-2 focus:ring-primary/20';
		}
		return 'border-(--border-subtle) bg-(--bg-surface) text-ink hover:border-muted focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20';
	});
</script>

<div class="relative">
	<select
		bind:this={selectRef}
		bind:value
		{disabled}
		{id}
		class="{baseClasses} {stateClasses()} {className}"
		onfocus={() => (isFocused = true)}
		onblur={() => (isFocused = false)}
		{...rest}
	>
		{#if placeholder}
			<option value="" disabled selected>{placeholder}</option>
		{/if}
		{#each options as option}
			<option value={option.value} disabled={option.disabled} title={option.title}>
				{option.label}
			</option>
		{/each}
	</select>

	<!-- Chevron Icon -->
	<div
		class="pointer-events-none absolute right-3 top-1/2 -translate-y-1/2 text-muted"
		aria-hidden="true"
	>
		<svg
			xmlns="http://www.w3.org/2000/svg"
			width="16"
			height="16"
			viewBox="0 0 24 24"
			fill="none"
			stroke="currentColor"
			stroke-width="2"
			stroke-linecap="round"
			stroke-linejoin="round"
		>
			<path d="m6 9 6 6 6-6" />
		</svg>
	</div>
</div>
