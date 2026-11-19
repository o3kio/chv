<script lang="ts">
	import type { PageDefinition } from '#lib/shell/app-shell.ts';
	import StatusBadge from '#lib/components/shell/StatusBadge.svelte';

	interface Props {
		page: PageDefinition;
		actions?: import('svelte').Snippet;
	}

	let { page, actions }: Props = $props();
</script>

<header class="grid gap-3">
	<div class="flex items-center gap-3 max-[720px]:items-start">
		<div class="grid place-items-center w-8 h-8 rounded-lg border border-(--shell-line) bg-(--shell-surface) text-(--shell-accent)" aria-hidden="true">
			<page.icon size={20}></page.icon>
		</div>
		<div>
			<div class="text-(length:--chv-text-xs) font-semibold tracking-[0.12em] uppercase text-(--shell-text-muted)">{page.eyebrow}</div>
			<h1 class="mt-[0.2rem] text-(length:--chv-text-3xl) leading-[1.05] text-(--shell-text)">{page.title}</h1>
		</div>
		{#if actions}
			<div class="flex ml-auto gap-2 items-center">
				{@render actions()}
			</div>
		{/if}
	</div>

	<div class="flex flex-wrap items-center justify-between gap-x-5 gap-y-[0.9rem]">
		<p class="max-w-3xl text-(length:--chv-text-sm) leading-relaxed text-(--shell-text-secondary)">{page.description}</p>
		<div class="flex flex-wrap gap-[0.55rem]">
			{#each page.badges as badge}
				<StatusBadge {...badge} />
			{/each}
		</div>
	</div>
</header>
