<script lang="ts">
	interface Props {
		startedMs: number;
		finishedMs?: number;
	}

	let { startedMs, finishedMs }: Props = $props();

	const duration = $derived.by(() => {
		if (!startedMs) return '-';
		const end = finishedMs || Date.now();
		const diff = end - startedMs;
		
		if (diff < 1000) return '< 1s';
		
		const seconds = Math.floor(diff / 1000);
		if (seconds < 60) return `${seconds}s`;
		
		const minutes = Math.floor(seconds / 60);
		const remainingSeconds = seconds % 60;
		
		if (minutes < 60) {
			return remainingSeconds > 0 ? `${minutes}m ${remainingSeconds}s` : `${minutes}m`;
		}
		
		const hours = Math.floor(minutes / 60);
		const remainingMinutes = minutes % 60;
		return `${hours}h ${remainingMinutes}m`;
	});
</script>

<span class="font-mono text-(length:--text-xs) {finishedMs ? 'text-(--shell-text-muted)' : 'text-(--shell-accent) font-medium'}">
	{duration}
</span>
