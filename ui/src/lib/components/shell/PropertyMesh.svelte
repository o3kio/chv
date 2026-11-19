<script lang="ts">
	interface Props {
		architecture?: string;
		providerType?: string;
		version?: string;
	}

	let { architecture = 'x86_64', providerType = 'LOCAL_HOST', version = 'v3.4.12-rc' }: Props = $props();
</script>

<div class="flex flex-col gap-2">
	<div class="flex justify-between text-[10px] pb-[0.35rem] border-b border-(--border-subtle)">
		<span class="text-neutral-500 font-bold">ARCHITECTURE</span>
		<span class="font-extrabold text-neutral-900 font-mono">{architecture}</span>
	</div>
	<div class="flex justify-between text-[10px] pb-[0.35rem] border-b border-(--border-subtle)">
		<span class="text-neutral-500 font-bold">FABRIC_DOMAIN</span>
		<span class="font-extrabold text-neutral-900 font-mono">{providerType}</span>
	</div>
	<div class="flex justify-between text-[10px] pb-[0.35rem] border-b border-(--border-subtle)">
		<span class="text-neutral-500 font-bold">VER_REGISTRY</span>
		<span class="font-extrabold text-neutral-900 font-mono">{version}</span>
	</div>
</div>
