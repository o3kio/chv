<script lang="ts">
	import StatusBadge from './StatusBadge.svelte';
	import type { ShellTone } from '#lib/shell/app-shell.ts';
	import { getTaskFailureCause } from '#lib/webui/task-failure.ts';
	import { ExternalLink } from 'lucide-svelte';

	interface Task {
		task_id: string;
		summary: string;
		status: string;
		operation: string;
		started_at?: string;
		tone: ShellTone;
		/** #502: the journaled terminal-failure cause — null until a failure records it. */
		error_code?: string | null;
		error_message?: string | null;
	}

	interface Props {
		tasks: Task[];
		emptyText?: string;
	}

	let { tasks, emptyText }: Props = $props();
</script>

<div class="flex flex-col gap-2">
	{#if tasks.length === 0}
		<p class="text-(length:--text-xs) text-(--shell-text-muted) py-2 text-center">{emptyText ?? 'No recent tasks recorded for this resource.'}</p>
	{:else}
		{#each tasks as task (task.task_id)}
			{@const failure = getTaskFailureCause(task)}
			<div class="flex justify-between items-center p-2 bg-(--shell-surface-muted) border border-(--shell-line) rounded-sm gap-4">
				<div class="flex flex-col gap-[0.15rem] min-w-0 flex-1">
					<div class="flex items-center gap-2 justify-between">
						<span class="font-semibold text-(length:--text-sm) text-(--shell-text) whitespace-nowrap overflow-hidden text-ellipsis">{task.summary}</span>
						<StatusBadge label={task.status} tone={task.tone} />
					</div>
					<div class="flex items-center gap-[0.35rem] text-(length:--text-xs) text-(--shell-text-muted)">
						<span>{task.operation}</span>
						{#if task.started_at}
							<span class="opacity-50">·</span>
							<span>{task.started_at}</span>
						{/if}
					</div>
					{#if failure}
						<!-- #502: the recorded terminal-failure cause, rendered as-is —
						     the code as a chip beside the status badge's vocabulary,
						     the refusal text verbatim (it IS the diagnostic). Absent
						     fields stay absent; nothing is fabricated. -->
						<div class="flex flex-col gap-[0.15rem] pt-1 mt-1 border-t border-(--shell-line)" data-testid="task-failure-cause">
							{#if failure.code}
								<StatusBadge label={failure.code} tone="failed" />
							{/if}
							{#if failure.message}
								<p class="text-(length:--text-xs) text-(--shell-text-muted) whitespace-pre-wrap wrap-break-word m-0">{failure.message}</p>
							{/if}
						</div>
					{/if}
				</div>
				<a href={`/tasks?query=${encodeURIComponent(task.task_id)}`} class="text-(--shell-text-muted) transition-colors duration-150 ease-in-out flex p-1 hover:text-(--shell-accent)" title="View detail">
					<ExternalLink size={14} />
				</a>
			</div>
		{/each}
	{/if}
</div>
