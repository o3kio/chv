<script lang="ts">
	interface Props {
		cpuUsage?: number;
		memoryUsagePercent?: number;
	}

	let { cpuUsage = 0, memoryUsagePercent = 0 }: Props = $props();
</script>

<div class="flex flex-col gap-3">
	<div class="flex flex-col gap-[0.35rem]">
		<div class="flex justify-between text-[9px] font-extrabold text-neutral-500">
			<span>CPU_PRESSURE</span>
			<span>{Math.round(cpuUsage)}%</span>
		</div>
		<div class="h-1 bg-(--bg-surface-muted) rounded-xs overflow-hidden">
			<div class="h-full bg-primary transition-[width] duration-300 ease-out" style="width: {cpuUsage}%"></div>
		</div>
	</div>
	<div class="flex flex-col gap-[0.35rem]">
		<div class="flex justify-between text-[9px] font-extrabold text-neutral-500">
			<span>RAM_RESERVATION</span>
			<span>{Math.round(memoryUsagePercent)}%</span>
		</div>
		<div class="h-1 bg-(--bg-surface-muted) rounded-xs overflow-hidden">
			<div class="h-full bg-primary transition-[width] duration-300 ease-out" style="width: {memoryUsagePercent}%"></div>
		</div>
	</div>
</div>
