<script lang="ts">
	interface Props {
		title: string;
		icon?: any;
		children: import('svelte').Snippet;
		actions?: import('svelte').Snippet;
		badgeLabel?: string;
		badgeTone?: 'healthy' | 'warning' | 'degraded' | 'failed' | 'neutral';
		collapsed?: boolean;
	}

	let { title, icon: Icon, children, actions, badgeLabel, badgeTone = 'neutral', collapsed = false }: Props = $props();

	const badgeToneClasses: Record<string, string> = {
		healthy: 'bg-(--color-success-light) text-(--color-success-dark)',
		warning: 'bg-(--color-warning-light) text-(--color-warning-dark)',
		degraded: 'bg-(--color-danger-light) text-(--color-danger-dark)',
		failed: 'bg-(--color-danger-light) text-(--color-danger-dark)',
		neutral: 'bg-(--shell-line) text-(--shell-text-muted)'
	};
</script>

<section class="bg-(--shell-surface) border border-(--shell-line) rounded-[0.35rem] flex flex-col overflow-hidden">
	<header class="flex justify-between items-center px-3 py-[0.65rem] border-b border-(--shell-line) bg-(--shell-surface-muted)">
		<div class="flex items-center gap-2">
			{#if Icon}<Icon size={14} class="text-(--shell-text-muted)" />{/if}
			<h3 class="text-(length:--text-xs) font-bold uppercase tracking-wider text-(--shell-text-muted) m-0">{title}</h3>
			{#if badgeLabel}
				<span class="text-[10px] font-bold px-[0.4rem] py-[0.1rem] rounded-full {badgeToneClasses[badgeTone]}">{badgeLabel}</span>
			{/if}
		</div>
		{#if actions}
			<div class="flex gap-2">
				{@render actions()}
			</div>
		{/if}
	</header>
	{#if !collapsed}
		<div class="p-3 text-(length:--text-sm)">
			{@render children()}
		</div>
	{/if}
</section>
