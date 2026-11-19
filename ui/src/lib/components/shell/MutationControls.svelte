<script lang="ts">
	import { Activity, Terminal, Settings } from 'lucide-svelte';
</script>

<div class="grid grid-cols-3 gap-2">
	<button type="button" class="flex flex-col items-center gap-2 px-2 py-3 bg-(--bg-surface) border border-(--border-subtle) rounded-xs text-[9px] font-extrabold text-neutral-600 cursor-pointer transition-all duration-150 ease-in-out disabled:opacity-50 disabled:cursor-not-allowed hover:not-disabled:bg-(--bg-surface-muted) hover:not-disabled:border-primary hover:not-disabled:text-primary" disabled title="Coming soon">
		<Activity size={14} />
		<span>SYNC</span>
	</button>
	<button type="button" class="flex flex-col items-center gap-2 px-2 py-3 bg-(--bg-surface) border border-(--border-subtle) rounded-xs text-[9px] font-extrabold text-neutral-600 cursor-pointer transition-all duration-150 ease-in-out disabled:opacity-50 disabled:cursor-not-allowed hover:not-disabled:bg-(--bg-surface-muted) hover:not-disabled:border-primary hover:not-disabled:text-primary" disabled title="Coming soon">
		<Terminal size={14} />
		<span>BYPASS_SHELL</span>
	</button>
	<button type="button" class="flex flex-col items-center gap-2 px-2 py-3 bg-(--bg-surface) border border-(--border-subtle) rounded-xs text-[9px] font-extrabold text-neutral-600 cursor-pointer transition-all duration-150 ease-in-out disabled:opacity-50 disabled:cursor-not-allowed hover:not-disabled:bg-(--bg-surface-muted) hover:not-disabled:border-primary hover:not-disabled:text-primary" disabled title="Coming soon">
		<Settings size={14} />
		<span>CONFIG</span>
	</button>
</div>
