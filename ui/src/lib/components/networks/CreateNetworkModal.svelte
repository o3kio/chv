<script lang="ts">
	import Modal from '#lib/components/primitives/Modal.svelte';
	import FormField from '#lib/components/shared/FormField.svelte';
	import Input from '#lib/components/primitives/TextInput.svelte';
	import Select from '#lib/components/primitives/Select.svelte';
	import { getStoredToken } from '#lib/api/client.ts';
	import { createNetwork, updateNetwork } from '#lib/bff/networks.ts';
	import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
	import type { CreateNetworkInput, NetworkDetailModel, UpdateNetworkInput } from '#lib/bff/types.ts';

	interface Props {
		open?: boolean;
		onSuccess?: () => void;
		editMode?: boolean;
		network?: NetworkDetailModel;
	}

	let { open = $bindable(false), onSuccess, editMode = false, network }: Props = $props();

	// Form state
	let name = $state('');
	let bridgeName = $state('chvbr0');
	let cidr = $state('10.0.0.0/24');
	let gateway = $state('10.0.0.1');
	let dhcpEnabled = $state(true);
	let ipamMode = $state('internal');
	let isDefault = $state(false);
	let submitting = $state(false);
	let formError = $state('');

	// Field-specific errors
	let nameError = $state('');
	let bridgeNameError = $state('');
	let cidrError = $state('');
	let gatewayError = $state('');

	// Validation regexes
	const nameRegex = /^[a-z0-9-]+$/;
	const cidrRegex = /^(\d{1,3}\.){3}\d{1,3}\/\d{1,2}$/;
	const ipRegex = /^(\d{1,3}\.){3}\d{1,3}$/;

	const ipamOptions = [
		{ value: 'internal', label: 'Internal' },
		{ value: 'external', label: 'External', disabled: true, title: 'Not available in this release' },
		{ value: 'none', label: 'None' }
	];

	function resetForm() {
		name = '';
		bridgeName = 'chvbr0';
		cidr = '10.0.0.0/24';
		gateway = '10.0.0.1';
		dhcpEnabled = true;
		ipamMode = 'internal';
		isDefault = false;
		formError = '';
		nameError = '';
		bridgeNameError = '';
		cidrError = '';
		gatewayError = '';
	}

	function validateName(): boolean {
		if (!name.trim()) {
			nameError = 'Name is required';
			return false;
		}
		if (!nameRegex.test(name)) {
			nameError = 'Name must contain only lowercase letters, numbers, and hyphens';
			return false;
		}
		if (name.startsWith('-') || name.endsWith('-')) {
			nameError = 'Name cannot start or end with a hyphen';
			return false;
		}
		nameError = '';
		return true;
	}

	function validateBridgeName(): boolean {
		if (!bridgeName.trim()) {
			bridgeNameError = 'Bridge name is required';
			return false;
		}
		bridgeNameError = '';
		return true;
	}

	function validateCidr(): boolean {
		if (!cidr.trim()) {
			cidrError = 'CIDR is required';
			return false;
		}
		if (!cidrRegex.test(cidr)) {
			cidrError = 'CIDR must be in format x.x.x.x/x (e.g., 10.0.0.0/24)';
			return false;
		}
		// Validate octets are within range
		const [ip, prefix] = cidr.split('/');
		const octets = ip.split('.').map(Number);
		if (octets.some((o) => o < 0 || o > 255)) {
			cidrError = 'IP octets must be between 0 and 255';
			return false;
		}
		const prefixNum = Number(prefix);
		if (prefixNum < 0 || prefixNum > 32) {
			cidrError = 'Prefix must be between 0 and 32';
			return false;
		}
		cidrError = '';
		return true;
	}

	function validateGateway(): boolean {
		if (!gateway.trim()) {
			gatewayError = 'Gateway IP is required';
			return false;
		}
		if (!ipRegex.test(gateway)) {
			gatewayError = 'Gateway must be a valid IP address (e.g., 10.0.0.1)';
			return false;
		}
		// Validate octets are within range
		const octets = gateway.split('.').map(Number);
		if (octets.some((o) => o < 0 || o > 255)) {
			gatewayError = 'IP octets must be between 0 and 255';
			return false;
		}
		gatewayError = '';
		return true;
	}

	function validate(): boolean {
		const validations = [validateName(), validateBridgeName(), validateCidr(), validateGateway()];
		return validations.every(Boolean);
	}

	function isValid(): boolean {
		// Quick check without setting errors - used for button disabled state
		if (!name.trim() || !bridgeName.trim() || !cidr.trim() || !gateway.trim()) {
			return false;
		}
		if (!nameRegex.test(name) || name.startsWith('-') || name.endsWith('-')) {
			return false;
		}
		if (!cidrRegex.test(cidr)) return false;
		if (!ipRegex.test(gateway)) return false;

		// Additional validation for octets
		const [ip] = cidr.split('/');
		if (ip.split('.').map(Number).some((o) => o < 0 || o > 255)) return false;
		if (gateway.split('.').map(Number).some((o) => o < 0 || o > 255)) return false;

		return true;
	}

	async function handleSubmit(event?: Event) {
		event?.preventDefault();

		if (!validate()) return;

		submitting = true;
		formError = '';

		try {
			if (editMode && network) {
				const data: UpdateNetworkInput = {
					network_id: network.network_id,
					name: name.trim(),
					cidr: cidr.trim(),
					gateway: gateway.trim(),
					dhcp_enabled: dhcpEnabled,
					ipam_mode: ipamMode as 'internal' | 'external' | 'none',
					is_default: isDefault
				};
				await mutateWithRefresh(
					() => updateNetwork(data, getStoredToken() ?? undefined),
					{
						patterns: ['networks:'],
						successMessage: `Network "${name}" updated successfully`,
						errorMessage: 'Failed to update network',
					}
				);
			} else {
				const data: CreateNetworkInput = {
					name: name.trim(),
					cidr: cidr.trim(),
					gateway: gateway.trim(),
					bridge_name: bridgeName.trim(),
					dhcp_enabled: dhcpEnabled,
					ipam_mode: ipamMode as 'internal' | 'external' | 'none',
					is_default: isDefault
				};
				await mutateWithRefresh(
					() => createNetwork(data, getStoredToken() ?? undefined),
					{
						patterns: ['networks:'],
						successMessage: `Network "${name}" created successfully`,
						errorMessage: 'Failed to create network',
					}
				);
			}
			open = false;
			onSuccess?.();
		} catch (err) {
			const message = err instanceof Error ? err.message : (editMode ? 'Failed to update network' : 'Failed to create network');
			formError = message;
		} finally {
			submitting = false;
		}
	}

	// Populate form in edit mode or reset on close
	$effect(() => {
		if (open) {
			if (editMode && network) {
				name = network.name;
				cidr = network.cidr;
				gateway = network.gateway;
				dhcpEnabled = network.dhcp_enabled;
				ipamMode = network.ipam_mode;
				isDefault = network.is_default;
			}
		} else {
			resetForm();
		}
	});
</script>

<Modal bind:open title={editMode ? 'Edit Network' : 'Create Network'} closeOnBackdrop={!submitting}>
	<form id="create-network-form" onsubmit={handleSubmit} class="space-y-5">
		{#if formError}
			<div class="rounded-sm border border-danger/30 bg-danger/10 px-3 py-2 text-sm text-danger" role="alert">
				{formError}
			</div>
		{/if}

		<FormField label="Name" error={nameError} required labelFor="network-name">
			<Input
				id="network-name"
				bind:value={name}
				placeholder="my-network"
				disabled={submitting}
				onblur={validateName}
			/>
		</FormField>

		<FormField label="Bridge Name" error={bridgeNameError} required labelFor="bridge-name">
			<Input
				id="bridge-name"
				bind:value={bridgeName}
				placeholder="chvbr0"
				disabled={submitting || editMode}
				onblur={validateBridgeName}
			/>
		</FormField>

		<FormField label="CIDR" error={cidrError} required labelFor="network-cidr">
			<Input
				id="network-cidr"
				bind:value={cidr}
				placeholder="10.0.0.0/24"
				disabled={submitting}
				onblur={validateCidr}
			/>
		</FormField>

		<FormField label="Gateway IP" error={gatewayError} required labelFor="gateway-ip">
			<Input
				id="gateway-ip"
				bind:value={gateway}
				placeholder="10.0.0.1"
				disabled={submitting}
				onblur={validateGateway}
			/>
		</FormField>

		<FormField label="IPAM Mode" helper="External IPAM is not available in this release." labelFor="ipam-mode">
			<Select id="ipam-mode" bind:value={ipamMode} options={ipamOptions} disabled={submitting} />
		</FormField>

		<FormField label="DHCP Enabled" labelFor="dhcp-enabled">
			<input
				id="dhcp-enabled"
				type="checkbox"
				bind:checked={dhcpEnabled}
				disabled={submitting}
				class="h-4 w-4 rounded-sm border-gray-300 text-primary focus:ring-primary"
			/>
		</FormField>

		<FormField label="Default Network" helper="Set as default for dev-install workloads" labelFor="is-default">
			<input
				id="is-default"
				type="checkbox"
				bind:checked={isDefault}
				disabled={submitting}
				class="h-4 w-4 rounded-sm border-gray-300 text-primary focus:ring-primary"
			/>
		</FormField>
	</form>

	{#snippet footer()}
		<button
			type="button"
			onclick={() => (open = false)}
			disabled={submitting}
			class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
		>
			Cancel
		</button>
		<button
			type="submit"
			form="create-network-form"
			disabled={!isValid() || submitting}
			class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed flex items-center gap-2"
		>
			{#if submitting}
				<svg
					class="animate-spin h-4 w-4"
					xmlns="http://www.w3.org/2000/svg"
					fill="none"
					viewBox="0 0 24 24"
					aria-hidden="true"
				>
					<circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
					<path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
				</svg>
			{/if}
			{submitting ? (editMode ? 'Saving...' : 'Creating...') : (editMode ? 'Save Changes' : 'Create Network')}
		</button>
	{/snippet}
</Modal>
