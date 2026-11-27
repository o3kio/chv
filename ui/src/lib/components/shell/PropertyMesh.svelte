<script lang="ts">
	interface Props {
		version?: string;
	}

	let { version = '' }: Props = $props();
</script>

{#if version}
	<div class="flex flex-col gap-2">
		<div class="flex justify-between text-[10px] pb-[0.35rem] border-b border-(--border-subtle)">
			<span class="text-(--chv-color-neutral-500) font-bold">Agent version</span>
			<span class="font-extrabold text-(--chv-color-neutral-900) font-mono">{version}</span>
		</div>
	</div>
{/if}
