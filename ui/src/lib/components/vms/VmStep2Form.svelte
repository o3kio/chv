<script lang="ts">
	import FormField from '#lib/components/shared/FormField.svelte';
	import Input from '#lib/components/primitives/TextInput.svelte';

	interface Props {
		username: string;
		sshKey: string;
		userData: string;
		submitting: boolean;
	}

	let {
		username = $bindable(),
		sshKey = $bindable(),
		userData = $bindable(),
		submitting
	}: Props = $props();
</script>

<div class="space-y-5">
	<FormField label="Username" required labelFor="vm-username">
		<Input
			id="vm-username"
			bind:value={username}
			placeholder="admin"
			disabled={submitting}
		/>
	</FormField>

	<FormField label="SSH Public Key" labelFor="vm-ssh-key">
		<textarea
			id="vm-ssh-key"
			bind:value={sshKey}
			placeholder="ssh-rsa AAAA..."
			class="w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 font-mono text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
			rows={3}
			disabled={submitting}
		></textarea>
	</FormField>

	<FormField label="User Data (cloud-init)" helper="Advanced cloud-config" labelFor="vm-userdata">
		<textarea
			id="vm-userdata"
			bind:value={userData}
			class="w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 font-mono text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
			rows={6}
			disabled={submitting}
		></textarea>
	</FormField>
</div>
