<script lang="ts">
	import Modal from '../primitives/Modal.svelte';
	import FormField from '../shared/FormField.svelte';
	import Input from '../primitives/Input.svelte';
	import { importImage } from '#lib/bff/images.ts';
	import { getStoredToken } from '#lib/api/client.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';

	interface Props {
		open?: boolean;
		onSuccess?: () => void;
	}

	let { open = $bindable(false), onSuccess }: Props = $props();

	let activeTab = $state<'remote' | 'local'>('remote');
	let name = $state('');
	let sourceUrl = $state('');
	let checksum = $state('');
	let osFamily = $state('linux');
	let architecture = $state('x86_64');
	let fileInput = $state<File | null>(null);
	let submitting = $state(false);
	let formError = $state('');

	function reset() {
		name = '';
		sourceUrl = '';
		checksum = '';
		osFamily = 'linux';
		architecture = 'x86_64';
		fileInput = null;
		formError = '';
	}

	async function handleSubmit() {
		formError = '';

		if (!name) {
			formError = 'Name is required';
			return;
		}

		if (activeTab === 'remote' && !sourceUrl) {
			formError = 'Source URL is required for remote import';
			return;
		}

		if (activeTab === 'local' && !fileInput) {
			formError = 'Please select a local file to upload';
			return;
		}

		submitting = true;
		try {
			const token = getStoredToken() ?? undefined;
			if (activeTab === 'remote') {
				await mutateWithRefresh(
					() => importImage({
						name,
						source_url: sourceUrl,
						checksum: checksum || undefined,
						os: osFamily,
						architecture: architecture,
						format: 'qcow2'
					}, token),
					{
						patterns: ['images:'],
						successMessage: 'Image import started',
						errorMessage: 'Action failed',
					}
				);
			} else {
				// Local file upload: for MVP we save metadata pointing to the file path.
				// A full upload endpoint would stream the file bytes to the server.
				await mutateWithRefresh(
					() => importImage({
						name,
						source_url: fileInput ? `file:///var/lib/chv/images/${fileInput.name}` : undefined,
						os: osFamily,
						architecture: architecture,
						format: 'qcow2'
					}, token),
					{
						patterns: ['images:'],
						successMessage: 'Image metadata registered. Copy the file to /var/lib/chv/images/ to complete.',
						errorMessage: 'Action failed',
					}
				);
			}

			open = false;
			reset();
			onSuccess?.();
		} catch (e: any) {
			formError = e.message || 'Action failed';
		} finally {
			submitting = false;
		}
	}

	function handleFileChange(e: Event) {
		const target = e.target as HTMLInputElement;
		if (target.files && target.files.length > 0) {
			fileInput = target.files[0];
			if (!name) name = fileInput.name.split('.')[0];
		}
	}

	$effect(() => {
		if (!open) reset();
	});
</script>

<Modal bind:open title="Import Image">
	<div class="tabs flex border-b border-line mb-4" role="tablist" aria-label="Import method">
		<button 
			type="button"
			role="tab"
			id="tab-remote"
			aria-selected={activeTab === 'remote'}
			aria-controls="panel-import-image"
			class="px-4 py-2 text-sm font-medium {activeTab === 'remote' ? 'border-b-2 border-accent text-accent' : 'text-muted'}"
			onclick={() => activeTab = 'remote'}
		>
			Remote URL
		</button>
		<button 
			type="button"
			role="tab"
			id="tab-local"
			aria-selected={activeTab === 'local'}
			aria-controls="panel-import-image"
			class="px-4 py-2 text-sm font-medium {activeTab === 'local' ? 'border-b-2 border-accent text-accent' : 'text-muted'}"
			onclick={() => activeTab = 'local'}
		>
			Local File
		</button>
	</div>

	<form onsubmit={handleSubmit} class="space-y-4">
		<div id="panel-import-image" role="tabpanel" aria-labelledby="tab-{activeTab}">
		{#if formError}
			<div class="error-banner">{formError}</div>
		{/if}

		<FormField label="Name" required>
			<Input bind:value={name} placeholder="ubuntu-22.04" />
		</FormField>

		{#if activeTab === 'remote'}
			<FormField label="Source URL" required helper="URL to qcow2 image">
				<Input bind:value={sourceUrl} placeholder="https://cloud-images.ubuntu.com/..." />
			</FormField>

			<FormField label="Checksum" helper="sha256:hash (optional)">
				<Input bind:value={checksum} placeholder="sha256:abc123..." />
			</FormField>
		{:else}
			<FormField label="Select File" required helper="Select a .qcow2 or .img file">
				<input 
					type="file" 
					accept=".qcow2,.img,.raw"
					onchange={handleFileChange}
					class="w-full text-sm text-gray-500 file:mr-4 file:py-2 file:px-4 file:rounded-full file:border-0 file:text-sm file:font-semibold file:bg-accent/10 file:text-accent hover:file:bg-accent/20 cursor-pointer"
				/>
			</FormField>
		{/if}

		<div class="grid grid-cols-2 gap-4">
			<FormField label="OS Family">
				<select bind:value={osFamily} class="w-full border border-line rounded-sm px-3 py-2 text-sm">
					<option value="linux">Linux</option>
				</select>
			</FormField>

			<FormField label="Architecture">
				<select bind:value={architecture} class="w-full border border-line rounded-sm px-3 py-2 text-sm">
					<option value="x86_64">x86_64</option>
					<option value="aarch64">aarch64</option>
				</select>
			</FormField>
		</div>

		<FormField label="Format">
			<Input value="qcow2" disabled />
			<p class="text-xs text-muted mt-1">Images are normalized to qcow2 for MVP-1</p>
		</FormField>
		</div>
	</form>

	{#snippet footer()}
		<button type="button" onclick={() => (open = false)} class="button-secondary px-4 py-2 rounded-sm text-sm">Cancel</button>
		<button type="button" onclick={handleSubmit} disabled={submitting} class="button-primary px-4 py-2 rounded-sm text-sm">
			{submitting ? 'Processing...' : (activeTab === 'remote' ? 'Start Import' : 'Upload Image')}
		</button>
	{/snippet}
</Modal>

<style>
	.error-banner {
		background: #fff0f0;
		border: 1px solid #e60000;
		color: #e60000;
		padding: 12px;
		border-radius: 4px;
	}
	.tabs button {
		transition: all 0.2s;
	}
</style>
