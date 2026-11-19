<script lang="ts">
	interface Props {
		progress: number;
		tone?: 'healthy' | 'warning' | 'failed' | 'neutral';
		size?: 'sm' | 'md' | 'lg';
	}

	let { progress, tone = 'healthy', size = 'md' }: Props = $props();

	const color = $derived({
		healthy: 'var(--color-success)',
		warning: 'var(--color-warning-dark)',
		failed: 'var(--color-danger)',
		neutral: 'var(--shell-accent)'
	}[tone]);
</script>

<div class="w-full h-1 bg-(--shell-line) rounded-full overflow-hidden">
	<div class="h-full transition-[width] duration-300 ease-out" style="width: {progress}%; background: {color}"></div>
</div>
