<script lang="ts">
	interface Props {
		interactive?: boolean;
		padding?: 'none' | 'sm' | 'md' | 'lg';
	}

	let {
		interactive = false,
		padding = 'md',
		children,
		header,
		footer
	}: Props & { children?: import('svelte').Snippet; header?: import('svelte').Snippet; footer?: import('svelte').Snippet } =
		$props();

	const paddingClasses = {
		none: '',
		sm: 'p-3',
		md: 'p-4',
		lg: 'p-6'
	};
</script>

<div
	class="bg-(--bg-surface) border border-neutral-200 rounded-md shadow-xs overflow-hidden transition-all duration-300 ease-in-out {interactive ? 'cursor-pointer hover:shadow-lg hover:border-primary/30 hover:-translate-y-0.5' : ''}"
	role="article"
>
	{#if header}
		<div class="border-b border-neutral-100 bg-(--chv-color-neutral-50) {paddingClasses[padding]}">
			{@render header()}
		</div>
	{/if}
	<div class="{paddingClasses[padding]}">
		{@render children?.()}
	</div>
	{#if footer}
		<div class="border-t border-neutral-100 bg-(--chv-color-neutral-50) {paddingClasses[padding]}">
			{@render footer()}
		</div>
	{/if}
</div>
