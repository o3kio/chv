<script lang="ts">
	interface Props {
		children: import('svelte').Snippet;
		title?: string;
	}

	let { children, title }: Props = $props();
</script>

<div class="flex flex-col gap-2">
	{#if title}<h3 class="text-(length:--text-xs) font-bold uppercase tracking-wider text-(--shell-text-muted) m-0">{title}</h3>{/if}
	<div class="flex flex-wrap gap-2 items-center">
		{@render children()}
	</div>
</div>
