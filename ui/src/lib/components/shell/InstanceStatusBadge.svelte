<script lang="ts">
	import type { InstanceStatus } from '#lib/api/types.ts';

	interface Props {
		status: InstanceStatus;
		showText?: boolean;
	}

	let { status, showText = true }: Props = $props();

	const config: Record<
		InstanceStatus,
		{ label: string; toneClass: string; dotClass: string }
	> = {
		running: {
			label: 'RUNNING',
			toneClass: 'text-(--status-healthy-text)',
			dotClass: 'bg-success'
		},
		stopped: {
			label: 'STOPPED',
			toneClass: 'text-(--chv-color-neutral-500)',
			dotClass: 'bg-(--chv-color-neutral-500)'
		},
		error: {
			label: 'ERROR',
			toneClass: 'text-(--status-failed-text)',
			dotClass: 'bg-danger'
		},
		paused: {
			label: 'PAUSED',
			toneClass: 'text-(--status-warning-text)',
			dotClass: 'bg-warning'
		},
		unknown: {
			label: 'UNKNOWN',
			toneClass: 'text-(--status-unknown-text)',
			dotClass: 'bg-(--chv-color-neutral-400)'
		}
	};

	const c = $derived(config[status] ?? config.unknown);
</script>

<span class="inline-flex items-center gap-1.5 text-(length:--chv-text-xs) font-medium {c.toneClass}" aria-label="Status: {c.label}">
	<span class="w-[6px] h-[6px] rounded-full {c.dotClass}" aria-hidden="true"></span>
	{#if showText}
		<span>{c.label}</span>
	{/if}
</span>
