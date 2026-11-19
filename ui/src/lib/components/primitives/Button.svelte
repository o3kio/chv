<script lang="ts">
	import type { HTMLButtonAttributes } from 'svelte/elements';
	import { Loader2 } from 'lucide-svelte';

	interface Props extends HTMLButtonAttributes {
		variant?: 'primary' | 'secondary' | 'ghost' | 'danger';
		size?: 'sm' | 'md' | 'lg';
		loading?: boolean;
		disabled?: boolean;
		ariaLabel?: string;
		title?: string;
	}

	let {
		variant = 'primary',
		size = 'md',
		loading = false,
		disabled = false,
		ariaLabel,
		title,
		children,
		...rest
	}: Props = $props();

	const isDisabled = $derived(disabled || loading);

	const variantClasses = {
		primary:
			'bg-linear-to-br from-primary to-(--color-primary-active) text-white shadow-[0_2px_8px_var(--color-primary-glow)] hover:shadow-[0_4px_12px_rgba(var(--color-primary-rgb),0.35)] hover:-translate-y-px active:translate-y-0',
		secondary:
			'bg-(--bg-surface) text-neutral-700 border border-neutral-300 hover:bg-(--color-neutral-50) hover:border-neutral-400',
		ghost:
			'bg-transparent text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900',
		danger: 'bg-danger text-white hover:bg-(--color-danger-dark)'
	};

	const sizeClasses = {
		sm: 'h-8 px-3 text-sm',
		md: 'h-10 px-4 text-sm',
		lg: 'h-12 px-5 text-base'
	};

	let buttonRef = $state<HTMLButtonElement | null>(null);
	let isIconOnly = $state(false);

	$effect(() => {
		if (buttonRef && !children) {
			isIconOnly = true;
		}
	});

	const iconOnlyClasses = $derived(
		isIconOnly
			? size === 'sm'
				? 'w-8 h-8 p-0'
				: size === 'lg'
					? 'w-12 h-12 p-0'
					: 'w-10 h-10 p-0'
			: ''
	);
</script>

<button
	bind:this={buttonRef}
	type="button"
	class="btn inline-flex items-center justify-center gap-2 rounded-xs border-none font-medium text-sm cursor-pointer transition-all duration-150 ease-in-out focus-visible:outline-2 focus-visible:outline-primary focus-visible:outline-offset-2 disabled:opacity-50 disabled:cursor-not-allowed {variantClasses[variant]} {sizeClasses[size]} {iconOnlyClasses}"
	disabled={isDisabled}
	aria-disabled={isDisabled}
	aria-busy={loading}
	aria-label={ariaLabel}
	{title}
	{...rest}
>
	{#if loading}
		<Loader2 size={size === 'lg' ? 20 : 16} aria-hidden="true" class="animate-spin" />
	{/if}
	{@render children?.()}
</button>

<style>
	@keyframes spin {
		from {
			transform: rotate(0deg);
		}
		to {
			transform: rotate(360deg);
		}
	}

	@media (prefers-contrast: high) {
		.btn {
			border: 1px solid currentColor;
		}

		.btn:focus-visible {
			outline: 3px solid currentColor;
			outline-offset: 2px;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.btn {
			transition: none;
		}

		.btn:hover:not(:disabled) {
			transform: none;
		}
	}
</style>
