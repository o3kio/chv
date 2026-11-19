<script lang="ts">
  import Modal from '../primitives/Modal.svelte';
  import FormField from '../shared/FormField.svelte';
  import Input from '../primitives/Input.svelte';
  import { createAPIClient } from '#lib/api/client.ts';
  import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
  import type { Quota, UserInfo } from '#lib/api/types.ts';

  interface Props {
    open?: boolean;
    quota?: Quota | null;
    users?: UserInfo[];
    onSuccess?: () => void;
  }

  let {
    open = $bindable(false),
    quota = null,
    users = [],
    onSuccess
  }: Props = $props();

  const client = createAPIClient();
  const isEditing = $derived(quota !== null);

  // Form state
  let userId = $state('');
  let maxVms = $state(10);
  let maxCpu = $state(20);
  let maxMemoryGb = $state(64);
  let maxStorageGb = $state(500);
  let maxNetworks = $state(5);

  let submitting = $state(false);
  let formError = $state('');

  // Validation errors
  let errors = $state<Record<string, string>>({});

  // Reset form when modal opens
  $effect(() => {
    if (open) {
      if (quota) {
        // Editing existing quota
        userId = quota.user_id;
        maxVms = quota.max_vms;
        maxCpu = quota.max_cpu;
        maxMemoryGb = quota.max_memory_gb;
        maxStorageGb = quota.max_storage_gb;
        maxNetworks = quota.max_networks;
      } else {
        // Creating new quota
        userId = '';
        maxVms = 10;
        maxCpu = 20;
        maxMemoryGb = 64;
        maxStorageGb = 500;
        maxNetworks = 5;
      }
      errors = {};
      formError = '';
    }
  });

  function validate(): boolean {
    errors = {};

    if (!isEditing && !userId) {
      errors.userId = 'User is required';
    }

    if (maxVms < 0) {
      errors.maxVms = 'Max VMs must be 0 or greater';
    }
    if (maxCpu < 0) {
      errors.maxCpu = 'Max CPU must be 0 or greater';
    }
    if (maxMemoryGb < 0) {
      errors.maxMemoryGb = 'Max memory must be 0 or greater';
    }
    if (maxStorageGb < 0) {
      errors.maxStorageGb = 'Max storage must be 0 or greater';
    }
    if (maxNetworks < 0) {
      errors.maxNetworks = 'Max networks must be 0 or greater';
    }

    return Object.keys(errors).length === 0;
  }

  async function handleSubmit(event?: Event) {
    event?.preventDefault();

    if (!validate()) {
      return;
    }

    submitting = true;
    formError = '';

    try {
      const data = {
        max_vms: maxVms,
        max_cpu: maxCpu,
        max_memory_gb: maxMemoryGb,
        max_storage_gb: maxStorageGb,
        max_networks: maxNetworks
      };

      if (isEditing && quota) {
        await mutateWithRefresh(
          () => client.updateQuota(quota.user_id, data),
          {
            successMessage: 'Quota updated successfully',
            errorMessage: 'Failed to save quota',
          }
        );
      } else {
        await mutateWithRefresh(
          () => client.createQuota({
            user_id: userId,
            ...data
          }),
          {
            successMessage: 'Quota created successfully',
            errorMessage: 'Failed to save quota',
          }
        );
      }

      open = false;
      onSuccess?.();
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to save quota';
      formError = message;
    } finally {
      submitting = false;
    }
  }

  function handleClose() {
    if (!submitting) {
      open = false;
    }
  }
</script>

<Modal bind:open title={isEditing ? 'Edit Quota' : 'Create Quota'} closeOnBackdrop={!submitting}>
  <form id="quota-form" class="space-y-5" onsubmit={handleSubmit}>
    {#if formError}
      <div
        class="rounded-sm border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-700"
        role="alert"
      >
        {formError}
      </div>
    {/if}

    {#if !isEditing}
      <FormField label="User" error={errors.userId} required labelFor="quota-user">
        <select
          id="quota-user"
          bind:value={userId}
          class="h-9 w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm disabled:opacity-50"
          disabled={submitting}
        >
          <option value="">Select a user...</option>
          {#each users as user}
            <option value={user.id}>{user.username} ({user.role})</option>
          {/each}
        </select>
      </FormField>
    {:else}
      <div class="rounded-sm bg-slate-50 px-3 py-2 text-sm text-slate-600">
        Editing quota for: <span class="font-medium">{quota?.user_id}</span>
      </div>
    {/if}

    <div class="grid grid-cols-2 gap-4">
      <FormField label="Max VMs" error={errors.maxVms} required labelFor="quota-vms">
        <Input
          id="quota-vms"
          type="number"
          bind:value={maxVms}
          min={0}
          disabled={submitting}
        />
      </FormField>

      <FormField label="Max CPU Cores" error={errors.maxCpu} required labelFor="quota-cpu">
        <Input
          id="quota-cpu"
          type="number"
          bind:value={maxCpu}
          min={0}
          disabled={submitting}
        />
      </FormField>
    </div>

    <div class="grid grid-cols-2 gap-4">
      <FormField label="Max Memory (GB)" error={errors.maxMemoryGb} required labelFor="quota-memory">
        <Input
          id="quota-memory"
          type="number"
          bind:value={maxMemoryGb}
          min={0}
          step={1}
          disabled={submitting}
        />
      </FormField>

      <FormField label="Max Storage (GB)" error={errors.maxStorageGb} required labelFor="quota-storage">
        <Input
          id="quota-storage"
          type="number"
          bind:value={maxStorageGb}
          min={0}
          step={10}
          disabled={submitting}
        />
      </FormField>
    </div>

    <FormField label="Max Networks" error={errors.maxNetworks} required labelFor="quota-networks">
      <Input
        id="quota-networks"
        type="number"
        bind:value={maxNetworks}
        min={0}
        disabled={submitting}
      />
    </FormField>

    <div class="rounded-sm bg-amber-50 border border-amber-200 px-3 py-2 text-sm text-amber-700">
      <p class="font-medium mb-1">Default Values</p>
      <p class="text-xs">
        VMs: 10, CPU: 20, Memory: 64GB, Storage: 500GB, Networks: 5
      </p>
    </div>
  </form>

  {#snippet footer()}
    <button
      type="button"
      onclick={handleClose}
      disabled={submitting}
      class="px-4 py-2 rounded-sm border border-slate-200 text-slate-700 bg-white hover:bg-slate-50 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
    >
      Cancel
    </button>
    <button
      type="submit"
      form="quota-form"
      disabled={submitting}
      class="px-4 py-2 rounded-sm bg-blue-600 text-white font-medium hover:bg-blue-700 transition-colors disabled:bg-blue-400 disabled:cursor-not-allowed flex items-center gap-2"
    >
      {#if submitting}
        <svg
          class="animate-spin h-4 w-4"
          xmlns="http://www.w3.org/2000/svg"
          fill="none"
          viewBox="0 0 24 24"
          aria-hidden="true"
        >
          <circle
            class="opacity-25"
            cx="12"
            cy="12"
            r="10"
            stroke="currentColor"
            stroke-width="4"
          ></circle>
          <path
            class="opacity-75"
            fill="currentColor"
            d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
          ></path>
        </svg>
      {/if}
      {submitting ? 'Saving...' : (isEditing ? 'Update Quota' : 'Create Quota')}
    </button>
  {/snippet}
</Modal>
