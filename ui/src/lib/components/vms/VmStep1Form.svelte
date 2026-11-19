<script lang="ts">
	import FormField from '#lib/components/shared/FormField.svelte';
	import Input from '#lib/components/primitives/TextInput.svelte';

	interface Props {
		name: string;
		imageId: string;
		networkId: string;
		vcpu: number;
		memoryMb: number;
		volumeSizeGb: number;
		nameError: string;
		images: { image_id: string; name: string }[];
		networks: { network_id: string; name: string; dhcp_enabled: boolean; ipam_mode: string; is_default: boolean }[];
		loadingData: boolean;
		advancedOpen: boolean;
		hvOverrides: Record<string, boolean | string | undefined>;
		vsockEnabled: boolean;
		submitting: boolean;
		onNameBlur?: () => void;
		onImageLinkClick?: () => void;
	}

	let {
		name = $bindable(),
		imageId = $bindable(),
		networkId = $bindable(),
		vcpu = $bindable(),
		memoryMb = $bindable(),
		volumeSizeGb = $bindable(),
		nameError = $bindable(),
		images,
		networks,
		loadingData,
		advancedOpen = $bindable(),
		hvOverrides = $bindable(),
		vsockEnabled = $bindable(),
		submitting,
		onNameBlur,
		onImageLinkClick
	}: Props = $props();

	const booleanOverrides = [
		{ key: 'cpu_nested', label: 'Nested Virtualization' },
		{ key: 'cpu_amx', label: 'AMX Acceleration' },
		{ key: 'cpu_kvm_hyperv', label: 'Hyper-V Enlightenments' },
		{ key: 'memory_mergeable', label: 'KSM Deduplication' },
		{ key: 'memory_hugepages', label: 'Hugepages' },
		{ key: 'memory_shared', label: 'Shared Memory' },
		{ key: 'memory_prefault', label: 'Memory Prefault' },
		{ key: 'iommu', label: 'IOMMU' },
		{ key: 'watchdog', label: 'Watchdog' },
		{ key: 'landlock_enable', label: 'Landlock Sandbox' },
		{ key: 'pvpanic', label: 'PvPanic Device' }
	];

	const textOverrides = [
		{ key: 'rng_src', label: 'RNG Source' },
		{ key: 'serial_mode', label: 'Serial Mode' },
		{ key: 'console_mode', label: 'Console Mode' },
		{ key: 'tpm_type', label: 'TPM Type' },
		{ key: 'tpm_socket_path', label: 'TPM Socket Path' }
	];
</script>

<div class="space-y-5">
	<FormField label="Name" error={nameError} required labelFor="vm-name">
		<Input
			id="vm-name"
			bind:value={name}
			placeholder="my-vm"
			disabled={submitting}
			onblur={onNameBlur}
		/>
	</FormField>

	<FormField label="Image" required labelFor="vm-image">
		{#if loadingData}
			<div class="text-sm text-muted">Loading images...</div>
		{:else if images.length === 0}
			<div class="rounded-sm border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning">
				No images available.
				<!-- svelte-ignore a11y_invalid_attribute -->
				<a href="/images" class="underline" onclick={(e) => { e.preventDefault(); onImageLinkClick?.(); }}>Go to Images</a>
				to import one first.
			</div>
		{:else}
			<select
				id="vm-image"
				bind:value={imageId}
				class="h-9 w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm"
				disabled={submitting}
			>
				<option value="">Select an image...</option>
				{#each images as img}
					<option value={img.image_id}>{img.name}</option>
				{/each}
			</select>
		{/if}
	</FormField>

	<FormField label="Network" required labelFor="vm-network">
		{#if loadingData}
			<div class="text-sm text-muted">Loading networks...</div>
		{:else if networks.length === 0}
			<select
				id="vm-network"
				bind:value={networkId}
				class="h-9 w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm"
				disabled={submitting}
			>
				<option value="default">default (auto-create)</option>
			</select>
			<p class="text-xs text-muted mt-1">A default network will be created automatically.</p>
		{:else}
			<select
				id="vm-network"
				bind:value={networkId}
				class="h-9 w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm"
				disabled={submitting}
			>
				<option value="">Select a network...</option>
				{#each networks as net}
					<option value={net.network_id}>{net.name}</option>
				{/each}
			</select>
		{/if}
	</FormField>

	<div class="grid grid-cols-2 gap-4">
		<FormField label="vCPUs" labelFor="vm-vcpu">
			<Input
				id="vm-vcpu"
				type="number"
				bind:value={vcpu}
				min={1}
				max={32}
				disabled={submitting}
			/>
		</FormField>
		<FormField label="Memory (MB)" labelFor="vm-memory">
			<Input
				id="vm-memory"
				type="number"
				bind:value={memoryMb}
				min={512}
				step={512}
				disabled={submitting}
			/>
		</FormField>
	</div>

	<FormField label="Disk Size (GB)" labelFor="vm-disk-size">
		<Input
			id="vm-disk-size"
			type="number"
			bind:value={volumeSizeGb}
			min={1}
			max={500}
			disabled={submitting}
		/>
	</FormField>

	<!-- Advanced hypervisor overrides -->
	<div class="pt-2">
		<button
			type="button"
			onclick={() => (advancedOpen = !advancedOpen)}
			class="flex items-center gap-2 text-sm font-medium text-primary hover:text-primary/80 transition-colors"
			disabled={submitting}
		>
			<span>Advanced Hypervisor Overrides</span>
			{#if advancedOpen}
				<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m18 15-6-6-6 6"/></svg>
			{:else}
				<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
			{/if}
		</button>

		{#if advancedOpen}
			<div class="mt-3 space-y-3 rounded-sm border border-line bg-chrome/40 p-3">
				<p class="text-xs text-muted">Select "Inherit" to use the global setting, or explicitly override for this VM.</p>

				<div class="flex items-center justify-between gap-3 border-b border-line pb-3">
					<span class="text-sm text-ink">
						Vsock Monitoring Transport
						<span class="block text-xs text-muted">
							Attach a virtio-vsock device for guest monitoring without a management network
						</span>
					</span>
					<input
						type="checkbox"
						checked={vsockEnabled}
						onchange={(e) => (vsockEnabled = e.currentTarget.checked)}
						class="h-4 w-4"
						disabled={submitting}
					/>
				</div>

				{#each booleanOverrides as item}
					<div class="flex items-center justify-between gap-3">
						<span class="text-sm text-ink">{item.label}</span>
						<select
							value={hvOverrides[item.key] === undefined ? '' : String(hvOverrides[item.key])}
							onchange={(e) => {
								const val = e.currentTarget.value;
								hvOverrides = {
									...hvOverrides,
									[item.key]: val === '' ? undefined : val === 'true'
								};
							}}
							class="h-8 rounded-sm border border-[#CCCCCC] bg-white px-2 py-1 text-sm"
							disabled={submitting}
						>
							<option value="">Inherit global</option>
							<option value="true">On</option>
							<option value="false">Off</option>
						</select>
					</div>
				{/each}

				<div class="pt-2 space-y-3">
					{#each textOverrides as item}
						<div class="flex items-center justify-between gap-3">
							<span class="text-sm text-ink">{item.label}</span>
							<input
								type="text"
								value={String(hvOverrides[item.key] ?? '')}
								oninput={(e) => {
									const val = e.currentTarget.value;
									hvOverrides = {
										...hvOverrides,
										[item.key]: val.trim() === '' ? undefined : val.trim()
									};
								}}
								placeholder="Inherit global"
								class="h-8 rounded-sm border border-[#CCCCCC] bg-white px-2 py-1 text-sm w-48"
								disabled={submitting}
							/>
						</div>
					{/each}
				</div>
			</div>
		{/if}
	</div>
</div>
