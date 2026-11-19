<script lang="ts">
  import Modal from '#lib/components/primitives/Modal.svelte';
  import FormField from '#lib/components/shared/FormField.svelte';
  import Input from '#lib/components/primitives/TextInput.svelte';
  import { createAPIClient, getStoredToken } from '#lib/api/client.ts';
  import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
  import type { VMTemplate, Image, Network, VM, CloudInitTemplate } from '#lib/api/types.ts';
  import { onMount } from 'svelte';

  interface Props {
    open?: boolean;
    template?: VMTemplate | null;
    images?: Image[];
    networks?: Network[];
    onSuccess?: () => void;
  }

  let {
    open = $bindable(false),
    template = null,
    images = [],
    networks = [],
    onSuccess
  }: Props = $props();

  const client = createAPIClient({ token: getStoredToken() ?? undefined });

  // Form state
  let name = $state('');
  let cloudInitTemplateId = $state('');
  let cloudInitVars = $state<Record<string, string>>({});
  let customUserData = $state('');
  let useCustomUserData = $state(false);

  // Loaded data
  let cloudInitTemplates: CloudInitTemplate[] = $state([]);
  let renderedPreview = $state('');
  let showPreview = $state(false);

  let submitting = $state(false);
  let formError = $state('');
  let nameError = $state('');

  const nameRegex = /^[a-z0-9-]+$/;

  onMount(async () => {
    try {
      cloudInitTemplates = await client.listCloudInitTemplates();
    } catch (e) {
      console.error('Failed to load cloud-init templates:', e);
    }
  });

  function resetForm() {
    name = '';
    cloudInitTemplateId = '';
    cloudInitVars = {};
    customUserData = '';
    useCustomUserData = false;
    renderedPreview = '';
    showPreview = false;
    formError = '';
    nameError = '';
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

  const selectedCloudInitTemplate = $derived(
    cloudInitTemplates.find(t => t.id === cloudInitTemplateId)
  );

  async function updatePreview() {
    if (!cloudInitTemplateId || useCustomUserData) {
      renderedPreview = customUserData;
      return;
    }

    try {
      const result = await client.renderCloudInitTemplate(cloudInitTemplateId, {
        variables: cloudInitVars
      });
      renderedPreview = result.rendered;
    } catch (e) {
      renderedPreview = customUserData || '# Error rendering template';
    }
  }

  async function handleSubmit() {
    if (!validateName()) return;
    if (!template) {
      formError = 'No template selected';
      return;
    }

    submitting = true;
    formError = '';

    try {
      const vm = await mutateWithRefresh(
        () => client.cloneFromTemplate(template.id, {
          name: name.trim(),
          variables: useCustomUserData ? {} : cloudInitVars,
          custom_user_data: useCustomUserData ? customUserData : undefined
        }),
        {
          patterns: ['vms:'],
          successMessage: 'VM created successfully',
          errorMessage: 'Failed to create VM',
        }
      );

      open = false;
      onSuccess?.();
      resetForm();
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to create VM';
      formError = message;
    } finally {
      submitting = false;
    }
  }

  // Update preview when variables change
  $effect(() => {
    if (cloudInitTemplateId && !useCustomUserData) {
      updatePreview();
    }
  });

  // Reset form when modal closes or template changes
  $effect(() => {
    if (!open) {
      resetForm();
    }
  });

  $effect(() => {
    if (template && open) {
      // Pre-fill with template defaults if available
      if (template.cloud_init_config) {
        customUserData = template.cloud_init_config;
      }
    }
  });
</script>

<Modal bind:open title="Create VM from Template" closeOnBackdrop={!submitting} width="wide">
  {#if template}
    <div class="space-y-5">
      <!-- Template Info -->
      <div class="bg-chrome rounded-lg p-4">
        <h3 class="text-sm font-semibold text-ink mb-2">Template: {template.name}</h3>
        <div class="grid grid-cols-2 gap-2 text-sm">
          <div class="text-muted">Resources:</div>
          <div class="text-ink">{template.vcpu} vCPU, {template.memory_mb} MB</div>
          <div class="text-muted">Image:</div>
          <div class="text-ink">{images.find(i => i.id === template?.image_id)?.name || template.image_id}</div>
          <div class="text-muted">Network:</div>
          <div class="text-ink">{networks.find(n => n.id === template?.network_id)?.name || template.network_id}</div>
        </div>
      </div>

      {#if formError}
        <div class="rounded-sm border border-danger/30 bg-danger/10 px-3 py-2 text-sm text-danger" role="alert">
          {formError}
        </div>
      {/if}

      <!-- VM Name -->
      <FormField label="VM Name" error={nameError} required labelFor="vm-name">
        <Input
          id="vm-name"
          bind:value={name}
          placeholder="my-new-vm"
          disabled={submitting}
          onblur={validateName}
        />
      </FormField>

      <!-- Cloud-init Configuration -->
      <div class="border-t border-line pt-4">
        <h3 class="text-sm font-semibold text-ink mb-3">Cloud-init Configuration</h3>

        <!-- Toggle between template and custom -->
        <div class="flex gap-4 mb-4">
          <label class="flex items-center gap-2 cursor-pointer">
            <input
              type="radio"
              bind:group={useCustomUserData}
              value={false}
              disabled={submitting}
              class="text-primary"
            />
            <span class="text-sm text-ink">Use Template</span>
          </label>
          <label class="flex items-center gap-2 cursor-pointer">
            <input
              type="radio"
              bind:group={useCustomUserData}
              value={true}
              disabled={submitting}
              class="text-primary"
            />
            <span class="text-sm text-ink">Custom User Data</span>
          </label>
        </div>

        {#if !useCustomUserData}
          <!-- Cloud-init Template Selector -->
          <FormField label="Cloud-init Template" labelFor="cloudinit-template">
            <select
              id="cloudinit-template"
              bind:value={cloudInitTemplateId}
              class="h-9 w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm"
              disabled={submitting}
            >
              <option value="">Select a template...</option>
              {#each cloudInitTemplates as cit}
                <option value={cit.id}>{cit.name}</option>
              {/each}
            </select>
          </FormField>

          <!-- Dynamic Variable Inputs -->
          {#if selectedCloudInitTemplate && selectedCloudInitTemplate.variables.length > 0}
            <div class="mt-4 space-y-3">
              <h4 class="text-xs font-medium text-muted uppercase tracking-wide">Template Variables</h4>
              {#each selectedCloudInitTemplate.variables as varName}
                <FormField label={varName} labelFor={`var-${varName}`}>
                  <Input
                    id={`var-${varName}`}
                    value={cloudInitVars[varName] || ''}
                    oninput={(e) => {
                      cloudInitVars = { ...cloudInitVars, [varName]: e.currentTarget.value };
                    }}
                    placeholder={`Enter ${varName}...`}
                    disabled={submitting}
                  />
                </FormField>
              {/each}
            </div>
          {/if}
        {:else}
          <!-- Custom User Data -->
          <FormField label="Custom User Data" helper="Raw cloud-config YAML" labelFor="custom-userdata">
            <textarea
              id="custom-userdata"
              bind:value={customUserData}
              placeholder="#cloud-config\n"
              class="w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 font-mono text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
              rows={8}
              disabled={submitting}
            ></textarea>
          </FormField>
        {/if}

        <!-- Preview Section -->
        <div class="mt-4">
          <button
            type="button"
            onclick={() => showPreview = !showPreview}
            class="text-sm text-primary hover:text-primary/80 font-medium"
          >
            {showPreview ? 'Hide Preview' : 'Show Preview'}
          </button>

          {#if showPreview}
            <div class="mt-2 rounded-sm bg-neutral-900 p-4 overflow-auto max-h-64">
              <pre class="text-xs text-neutral-300 font-mono whitespace-pre-wrap">{renderedPreview || customUserData || '# No configuration'}</pre>
            </div>
          {/if}
        </div>
      </div>
    </div>
  {:else}
    <div class="text-center py-8 text-muted">
      No template selected
    </div>
  {/if}

  {#snippet footer()}
    <button
      type="button"
      onclick={() => open = false}
      disabled={submitting}
      class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
    >
      Cancel
    </button>
    <button
      type="button"
      onclick={handleSubmit}
      disabled={submitting || !name.trim()}
      class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed flex items-center gap-2"
    >
      {#if submitting}
        <svg class="animate-spin h-4 w-4" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" aria-hidden="true">
          <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
          <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
        </svg>
      {/if}
      {submitting ? 'Creating...' : 'Create VM'}
    </button>
  {/snippet}
</Modal>
