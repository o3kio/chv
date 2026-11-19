<script lang="ts">
	import { Database } from 'lucide-svelte';
</script>

<div class="flex-1 flex flex-col items-center justify-center p-8 text-center gap-6">
	<div class="text-(--chv-color-neutral-200)">
		<Database size={32} />
	</div>
	<p class="text-(length:--chv-text-sm) font-medium text-(--shell-text-muted) max-w-[16rem] leading-relaxed">Select a node or VM to inspect its live status and metadata.</p>
</div>
