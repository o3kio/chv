<script lang="ts">
	interface Props {
		name: string;
		selectedImageName: string;
		selectedNetworkName: string;
		volumeSizeGb: number;
		vcpu: number;
		memoryMb: number;
		username: string;
		vsockEnabled: boolean;
		overrideSummary: string;
	}

	let {
		name,
		selectedImageName,
		selectedNetworkName,
		volumeSizeGb,
		vcpu,
		memoryMb,
		username,
		vsockEnabled,
		overrideSummary
	}: Props = $props();
</script>

<div class="space-y-5">
	<h3 class="text-base font-semibold text-ink">Review Configuration</h3>

	<div class="space-y-3 text-sm">
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Name:</span>
			<span class="font-medium text-ink">{name}</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Image:</span>
			<span class="font-medium text-ink">{selectedImageName}</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Network:</span>
			<span class="font-medium text-ink">{selectedNetworkName}</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Disk:</span>
			<span class="font-medium text-ink">{volumeSizeGb} GB</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Resources:</span>
			<span class="font-medium text-ink">{vcpu} vCPU, {memoryMb} MB</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Username:</span>
			<span class="font-medium text-ink">{username}</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Hypervisor:</span>
			<span class="font-medium text-ink">{overrideSummary}</span>
		</div>
		<div class="flex justify-between border-b border-line pb-2">
			<span class="text-muted">Vsock Monitoring:</span>
			<span class="font-medium text-ink">{vsockEnabled ? 'Enabled' : 'Off'}</span>
		</div>
	</div>

	<div class="rounded-sm bg-chrome p-4 text-xs text-muted">
		<p class="font-medium mb-2">This will create:</p>
		<ul class="ml-4 list-disc space-y-1">
			<li>VM with {vcpu} vCPU, {memoryMb} MB RAM</li>
			<li>{volumeSizeGb} GB boot disk</li>
			<li>Network interface on {selectedNetworkName}</li>
		</ul>
	</div>
</div>
