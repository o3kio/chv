<script lang="ts">
	interface Props {
		variant?: 'default' | 'success' | 'warning' | 'danger' | 'info';
		dot?: boolean;
		animate?: boolean;
	}

	let {
		variant = 'default',
		dot = false,
		animate = false,
		children
	}: Props & { children?: import('svelte').Snippet } = $props();

	const variantClasses = {
		default: 'bg-neutral-100 text-neutral-600 border-neutral-200',
		success: 'bg-success/15 text-(--color-success-dark) border-success/20',
		warning: 'bg-warning/15 text-(--color-warning-dark) border-warning/20',
		danger: 'bg-danger/15 text-(--color-danger-dark) border-danger/20',
		info: 'bg-(--color-info)/15 text-(--color-info-dark) border-(--color-info)/20'
	};

	const dotClasses = {
		default: 'bg-neutral-400',
		success: 'bg-success shadow-(--shadow-glow-success)',
		warning: 'bg-warning',
		danger: 'bg-danger',
		info: 'bg-(--color-info)'
	};
</script>

<span
	class="inline-flex items-center gap-1 px-2.5 py-0.5 text-xs font-semibold uppercase tracking-wide rounded-full border {variantClasses[variant]}"
	role="status"
>
	{#if dot}
		<span class="w-1.5 h-1.5 rounded-full {dotClasses[variant]} {animate ? 'animate-pulse' : ''}" aria-hidden="true"
		></span>
	{/if}
	{@render children?.()}
</span>
