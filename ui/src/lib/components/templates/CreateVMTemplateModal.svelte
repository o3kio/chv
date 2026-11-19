<script lang="ts">
  import type { CloudInitTemplate, VM } from '#lib/api/types.ts';

  interface Props {
    open: boolean;
    newTemplateName: string;
    newTemplateDescription: string;
    selectedVMId: string;
    selectedCloudInitId: string;
    creatingTemplate: boolean;
    vms: VM[];
    cloudInitTemplates: CloudInitTemplate[];
    handleCreateVMTemplate: () => void;
  }

  let {
    open = $bindable(),
    newTemplateName = $bindable(),
    newTemplateDescription = $bindable(),
    selectedVMId = $bindable(),
    selectedCloudInitId = $bindable(),
    creatingTemplate,
    vms,
    cloudInitTemplates,
    handleCreateVMTemplate
  }: Props = $props();
</script>

{#if open}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    tabindex="-1"
    aria-modal="true"
    aria-labelledby="create-template-title"
    onclick={(e) => {
      if (e.target === e.currentTarget) open = false;
    }}
    onkeydown={(e) => {
      if (e.key === 'Escape') open = false;
    }}
  >
    <div class="bg-white rounded-lg shadow-lg w-full max-w-lg mx-4">
      <div class="flex items-center justify-between px-6 py-4 border-b border-line">
        <h2 id="create-template-title" class="text-lg font-semibold text-ink">Create VM Template</h2>
        <button
          type="button"
          onclick={() => open = false}
          class="text-muted hover:text-ink"
          aria-label="Close dialog"
        >
          <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>
        </button>
      </div>

      <div class="p-6 space-y-4">
        <div>
          <label for="template-name" class="block text-sm font-medium text-ink mb-1">
            Template Name <span class="text-danger">*</span>
          </label>
          <input
            id="template-name"
            type="text"
            bind:value={newTemplateName}
            placeholder="e.g., Ubuntu Web Server"
            class="w-full h-9 rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
          />
        </div>

        <div>
          <label for="template-description" class="block text-sm font-medium text-ink mb-1">
            Description
          </label>
          <input
            id="template-description"
            type="text"
            bind:value={newTemplateDescription}
            placeholder="Brief description of this template"
            class="w-full h-9 rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
          />
        </div>

        <div>
          <label for="source-vm" class="block text-sm font-medium text-ink mb-1">
            Source VM <span class="text-danger">*</span>
          </label>
          <select
            id="source-vm"
            bind:value={selectedVMId}
            class="w-full h-9 rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
          >
            <option value="">Select a VM...</option>
            {#each vms as vm}
              <option value={vm.id}>{vm.name} ({vm.vcpu} vCPU, {vm.memory_mb} MB)</option>
            {/each}
          </select>
          {#if vms.length === 0}
            <p class="text-xs text-muted mt-1">No VMs available. Create a VM first to use as a template.</p>
          {/if}
        </div>

        <div>
          <label for="cloud-init-template" class="block text-sm font-medium text-ink mb-1">
            Default Cloud-init Template (Optional)
          </label>
          <select
            id="cloud-init-template"
            bind:value={selectedCloudInitId}
            class="w-full h-9 rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
          >
            <option value="">None</option>
            {#each cloudInitTemplates as cit}
              <option value={cit.id}>{cit.name}</option>
            {/each}
          </select>
          <p class="text-xs text-muted mt-1">
            This cloud-init config will be used by default when cloning from this template.
          </p>
        </div>
      </div>

      <div class="flex items-center justify-end gap-2 px-6 py-4 border-t border-line">
        <button
          type="button"
          onclick={() => open = false}
          disabled={creatingTemplate}
          class="px-4 py-2 rounded-sm border border-line text-ink bg-white hover:bg-chrome transition-colors disabled:opacity-50"
        >
          Cancel
        </button>
        <button
          type="button"
          onclick={handleCreateVMTemplate}
          disabled={creatingTemplate || !newTemplateName.trim() || !selectedVMId}
          class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed flex items-center gap-2"
        >
          {#if creatingTemplate}
            <svg class="animate-spin h-4 w-4" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24">
              <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
              <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
            </svg>
            Creating...
          {:else}
            Create Template
          {/if}
        </button>
      </div>
    </div>
  </div>
{/if}
