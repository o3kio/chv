<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
	import { Check, RotateCcw } from 'lucide-svelte';
	import SectionCard from '#lib/components/shell/SectionCard.svelte';
	import ConfirmAction from '#lib/components/shared/ConfirmAction.svelte';
	import type { HypervisorProfile } from '#lib/bff/hypervisor-settings.ts';

	interface Props {
		profiles: HypervisorProfile[];
		currentProfileId: string | null;
		selectedProfileId: string;
		saving: boolean;
		onApply: (profileId: string) => void;
		onReset: () => void;
	}

	let {
		profiles,
		currentProfileId,
		selectedProfileId = $bindable(),
		saving,
		onApply,
		onReset
	}: Props = $props();

	let confirmApplyOpen = $state(false);
	let confirmResetOpen = $state(false);

	const currentProfileName = $derived(
		profiles.find((p) => p.id === currentProfileId)?.name ?? 'CUSTOM_VARS'
	);
</script>

<SectionCard title="Fabric Profiles" icon={Check}>
	<div class="profile-ops">
		<div class="field-control">
			<label class="field-label" for="registry-preset">REGISTRY_PRESET</label>
			<select id="registry-preset" bind:value={selectedProfileId}>
				<option value="">—</option>
				{#each profiles as profile}
					<option value={profile.id}>{profile.name}</option>
				{/each}
			</select>
		</div>

		<Button variant="primary" class="w-full" disabled={saving || !selectedProfileId} onclick={() => confirmApplyOpen = true}>
			<Check size={14} />
			APPLY_PRESET
		</Button>
		<Button variant="secondary" class="w-full" disabled={saving} onclick={() => confirmResetOpen = true}>
			<RotateCcw size={14} />
			RESET_DEFAULTS
		</Button>
	</div>
</SectionCard>

<SectionCard title="Active Profile" icon={Check}>
	<div class="safety-sign">
		<Check size={16} />
		<span>Current profile: {currentProfileName}</span>
	</div>
</SectionCard>

<ConfirmAction
	bind:open={confirmApplyOpen}
	title="Apply Fabric Profile"
	description="This will overwrite all global fabric parameters with the preset registry. Continue?"
	actionType="generic"
	confirmText="Commit Changes"
	onConfirm={() => {
		confirmApplyOpen = false;
		onApply(selectedProfileId);
	}}
/>

<ConfirmAction
	bind:open={confirmResetOpen}
	title="Restore Defaults"
	description="This will reset all fabric parameters to the initial safe state. Continue?"
	actionType="generic"
	confirmText="Restore Registry"
	onConfirm={() => {
		confirmResetOpen = false;
		onReset();
	}}
/>

<style>
	.profile-ops {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}

	.field-control {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}

	.field-label {
		font-size: 10px;
		font-weight: 800;
		color: var(--chv-color-neutral-500);
	}

	select {
		background: var(--bg-surface-muted);
		border: 1px solid var(--border-subtle);
		border-radius: var(--radius-xs);
		padding: 0.35rem 0.5rem;
		font-size: 11px;
		font-family: var(--chv-font-mono);
		color: var(--chv-color-neutral-900);
	}

	.safety-sign {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		padding: 0.75rem;
		background: rgba(var(--color-success-rgb), 0.1);
		color: var(--color-success);
		font-size: 10px;
		font-weight: 800;
		border-radius: var(--radius-xs);
	}


</style>
