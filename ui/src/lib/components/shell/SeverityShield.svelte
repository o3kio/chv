<script lang="ts">
	import { Info, AlertTriangle, ShieldAlert } from 'lucide-svelte';

	interface Props {
		severity: 'info' | 'warning' | 'critical';
		showLabel?: boolean;
	}

	let { severity, showLabel = false }: Props = $props();

	const config = $derived({
		info: { icon: Info, color: 'var(--shell-accent)', label: 'Info', bg: 'var(--shell-surface-muted)' },
		warning: { icon: AlertTriangle, color: 'var(--color-warning-dark)', label: 'Warning', bg: 'var(--color-warning-light)' },
		critical: { icon: ShieldAlert, color: 'var(--color-danger)', label: 'Critical', bg: 'var(--color-danger-light)' }
	}[severity]);
</script>

<div class="inline-flex items-center gap-[0.35rem] px-[0.4rem] py-[0.15rem] rounded-full font-bold text-[10px] uppercase tracking-wider {severity === 'critical' ? 'border border-danger shadow-[0_0_4px_rgba(239,68,68,0.2)]' : severity === 'warning' ? 'border border-(--color-warning-dark)' : 'border border-transparent'}" style="background: {config.bg}; color: {config.color};">
	<config.icon size={14} />
	{#if showLabel}
		<span>{config.label}</span>
	{/if}
</div>
