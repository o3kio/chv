<script lang="ts">
	import type { ShellTone } from '#lib/shell/app-shell.ts';

	interface Props {
		label: string;
		tone: ShellTone;
	}

	let { label, tone }: Props = $props();

	const toneClasses: Record<string, string> = {
		healthy: 'bg-(--status-healthy-bg) border-(--status-healthy-border) text-(--status-healthy-text)',
		warning: 'bg-(--status-warning-bg) border-(--status-warning-border) text-(--status-warning-text)',
		degraded: 'bg-(--status-degraded-bg) border-(--status-degraded-border) text-(--status-degraded-text)',
		failed: 'bg-(--status-failed-bg) border-(--status-failed-border) text-(--status-failed-text)',
		unknown: 'bg-(--status-unknown-bg) border-(--status-unknown-border) text-(--status-unknown-text)'
	};
</script>

<span class="inline-flex items-center border rounded-full px-2 py-[0.15rem] text-(length:--chv-text-xs) font-semibold tracking-[0.08em] uppercase whitespace-nowrap {toneClasses[tone]}">{label}</span>
