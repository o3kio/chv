<script lang="ts">
	import { Info, X } from 'lucide-svelte';
	import { selection } from '#lib/stores/selection.svelte.ts';
	import Button from '#lib/components/primitives/Button.svelte';
</script>

<header class="px-3 py-2 border-b border-(--border-subtle) flex justify-between items-center bg-(--bg-surface-muted)">
	<div class="flex items-center gap-2 text-(length:--chv-text-xs) font-bold text-(--shell-text-secondary) tracking-[0.06em] uppercase">
		<Info size={12} />
		<span>Quick inspector</span>
	</div>
	<Button variant="ghost" ariaLabel="Close inspector" onclick={() => selection.clear()}>
		<X size={12} />
	</Button>
</header>
