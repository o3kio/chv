<script lang="ts">
import Button from '#lib/components/primitives/Button.svelte';
  import { onMount } from 'svelte';
  import {
    FileCode, Box, LayoutTemplate
  } from 'lucide-svelte';
  import { createAPIClient, getStoredToken } from '#lib/api/client.ts';
  import {
    loadImagesFromBff,
    loadNetworksFromBff,
    loadVmsFromBff
  } from '#lib/webui/bff-resources.ts';
  import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
  import CompactMetricCard from '#lib/components/shared/CompactMetricCard.svelte';
  import PageHeaderWithAction from '#lib/components/shell/PageHeaderWithAction.svelte';
  import CreateFromTemplate from '#lib/components/vms/CreateFromTemplate.svelte';
  import CloudInitModalViewer from '#lib/components/shell/CloudInitModalViewer.svelte';
  import CloudInitModalEditor from '#lib/components/shell/CloudInitModalEditor.svelte';
  import { getPageDefinition } from '#lib/shell/app-shell.ts';
  import type { VMTemplate, CloudInitTemplate, Image, Network, VM } from '#lib/api/types.ts';
  import ConfirmDialog from '#lib/components/shared/ConfirmDialog.svelte';
  import TemplatesTable from '#lib/components/templates/TemplatesTable.svelte';
  import TemplatesSidebar from '#lib/components/templates/TemplatesSidebar.svelte';
  import CreateVMTemplateModal from '#lib/components/templates/CreateVMTemplateModal.svelte';

  const client = createAPIClient();
  const pageDef = getPageDefinition('/images'); // Reusing Images definition as it covers library

  let vmTemplates = $state<VMTemplate[]>([]);
  let cloudInitTemplates = $state<CloudInitTemplate[]>([]);
  let images = $state<Image[]>([]);
  let networks = $state<Network[]>([]);
  let vms = $state<VM[]>([]);
  let loading = $state(true);
  let error = $state('');
  let activeTab = $state<'vm' | 'cloudinit'>('vm');

  // Modal states
  let createFromTemplateOpen = $state(false);
  let selectedTemplate = $state<VMTemplate | null>(null);
  let cloudInitViewerOpen = $state(false);
  let cloudInitEditorOpen = $state(false);
  let selectedCloudInitTemplate = $state<CloudInitTemplate | null>(null);
  let createVMTemplateOpen = $state(false);
  let newTemplateName = $state('');
  let newTemplateDescription = $state('');
  let selectedVMId = $state('');
  let selectedCloudInitId = $state('');
  let creatingTemplate = $state(false);
  let confirmDialog = $state({ open: false, title: '', description: '', action: () => {} });

  async function handleCreateVMTemplate() {
    if (!newTemplateName.trim() || !selectedVMId) return;
    creatingTemplate = true;
    try {
      await mutateWithRefresh(
        () => client.createVMTemplate({
          name: newTemplateName.trim(),
          description: newTemplateDescription.trim() || undefined,
          source_vm_id: selectedVMId,
          cloud_init_config: selectedCloudInitId ? undefined : undefined
        }),
        {
          patterns: ['templates:'],
          successMessage: 'Template created successfully',
          errorMessage: 'Failed to create template',
          skipRefresh: true,
        }
      );
      createVMTemplateOpen = false;
      newTemplateName = '';
      newTemplateDescription = '';
      selectedVMId = '';
      selectedCloudInitId = '';
      await loadData();
    } catch (err: any) {
      // Error already toasted by mutateWithRefresh
    } finally {
      creatingTemplate = false;
    }
  }

  const vmColumns = [
    { key: 'name', label: 'Template Identity' },
    { key: 'resources', label: 'Resource Profile' },
    { key: 'image_name', label: 'Base Image' },
    { key: 'tags', label: 'Directives' },
    { key: 'status', label: 'Availability', align: 'center' as const },
    { key: '_actions', label: '', align: 'center' as const }
  ];

  const ciColumns = [
    { key: 'name', label: 'Identity' },
    { key: 'variables', label: 'Defined Var Registry' },
    { key: 'last_used', label: 'Last Seq', align: 'right' as const },
    { key: '_actions', label: '', align: 'center' as const }
  ];

  async function loadData() {
    loading = true;
    try {
      const [vmTemps, cloudTemps, imgs, nets, vmList] = await Promise.all([
        client.listVMTemplates(),
        client.listCloudInitTemplates(),
        loadImagesFromBff(getStoredToken() ?? undefined),
        loadNetworksFromBff(getStoredToken() ?? undefined),
        loadVmsFromBff(getStoredToken() ?? undefined)
      ]);
      vmTemplates = vmTemps ?? [];
      cloudInitTemplates = cloudTemps ?? [];
      images = imgs ?? [];
      networks = nets ?? [];
      vms = vmList ?? [];
    } catch (err: any) {
      error = err.message || 'Blueprint registry unavailable';
    } finally {
      loading = false;
    }
  }

  onMount(loadData);

  async function refreshTemplates() {
    await loadData();
  }

  function cloneTemplate(template: VMTemplate) {
    selectedTemplate = template;
    createFromTemplateOpen = true;
  }
</script>

<div class="inventory-page">
  <PageHeaderWithAction page={pageDef}>
    {#snippet actions()}
      <div class="header-actions">
        {#if activeTab === 'vm'}
          <Button variant="primary" onclick={() => createVMTemplateOpen = true}>
            <LayoutTemplate size={14} />
            Commit Blueprint
          </Button>
        {:else}
          <Button variant="primary" onclick={() => cloudInitEditorOpen = true}>
            <FileCode size={14} />
            Register Init Script
          </Button>
        {/if}
      </div>
    {/snippet}
  </PageHeaderWithAction>

  <div class="inventory-metrics">
    <CompactMetricCard
      label="Provision Blueprints"
      value={vmTemplates.length}
      color="neutral"
    />
    <CompactMetricCard
      label="Init Registries"
      value={cloudInitTemplates.length}
      color="primary"
    />
    <CompactMetricCard
      label="Library Assets"
      value={images.length}
      color="neutral"
    />
    <CompactMetricCard
      label="SLA Compliance"
      value="NOMINAL"
      color="primary"
    />
  </div>

  <div class="tabs-nav" role="tablist" aria-label="Template types">
    <button type="button" role="tab" id="tab-vm" aria-selected={activeTab === 'vm'} aria-controls="panel-templates" class="tab-item" class:active={activeTab === 'vm'} onclick={() => activeTab = 'vm'}>
      <Box size={14} />
      <span>Workload Blueprints</span>
    </button>
    <button type="button" role="tab" id="tab-cloudinit" aria-selected={activeTab === 'cloudinit'} aria-controls="panel-templates" class="tab-item" class:active={activeTab === 'cloudinit'} onclick={() => activeTab = 'cloudinit'}>
      <FileCode size={14} />
      <span>Init Registries</span>
    </button>
  </div>

  <main class="inventory-main">
    <div id="panel-templates" role="tabpanel" aria-labelledby="tab-{activeTab}" class="contents">
      <TemplatesTable
        {activeTab}
        {loading}
        {error}
        {vmTemplates}
        {cloudInitTemplates}
        {images}
        {vmColumns}
        {ciColumns}
        {cloneTemplate}
      />
      <TemplatesSidebar />
    </div>
  </main>
</div>

{#if createFromTemplateOpen}
  <CreateFromTemplate
    bind:open={createFromTemplateOpen}
    template={selectedTemplate}
    {images}
    {networks}
    onSuccess={refreshTemplates}
  />
{/if}



<!-- Create From Template Modal -->
<CreateFromTemplate
  bind:open={createFromTemplateOpen}
  template={selectedTemplate}
  {images}
  {networks}
  onSuccess={refreshTemplates}
/>

<!-- Cloud-init Viewer Modal -->
<CloudInitModalViewer
  bind:open={cloudInitViewerOpen}
  template={selectedCloudInitTemplate}
/>

<!-- Cloud-init Editor Modal -->
<CloudInitModalEditor
  bind:open={cloudInitEditorOpen}
  onSuccess={refreshTemplates}
/>

<!-- Create VM Template Modal -->
<CreateVMTemplateModal
  bind:open={createVMTemplateOpen}
  bind:newTemplateName
  bind:newTemplateDescription
  bind:selectedVMId
  bind:selectedCloudInitId
  {creatingTemplate}
  {vms}
  {cloudInitTemplates}
  {handleCreateVMTemplate}
/>

<ConfirmDialog
  bind:open={confirmDialog.open}
  title={confirmDialog.title}
  description={confirmDialog.description}
  confirmText="Delete"
  variant="danger"
  onConfirm={() => { confirmDialog.action(); confirmDialog.open = false; }}
  onCancel={() => confirmDialog.open = false}
/>

<style>
  .inventory-page {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  .header-actions {
    display: flex;
    gap: 0.5rem;
  }

  .inventory-metrics {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 0.75rem;
  }

  .tabs-nav {
    display: flex;
    gap: 0.25rem;
    padding: 0.25rem;
    background: var(--bg-surface-muted);
    border-radius: var(--radius-xs);
    width: fit-content;
  }

  .tab-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.35rem 0.75rem;
    font-size: 10px;
    font-weight: 700;
    color: var(--chv-color-neutral-500);
    text-transform: uppercase;
    border-radius: var(--radius-xs);
    transition: all 0.1s ease;
  }

  .tab-item:hover {
    color: var(--chv-color-neutral-900);
  }

  .tab-item.active {
    background: var(--bg-surface);
    color: var(--color-primary);
    box-shadow: 0 1px 2px rgba(0,0,0,0.05);
  }

  .inventory-main {
    display: grid;
    grid-template-columns: 1fr 300px;
    gap: 1rem;
    align-items: start;
  }

  @media (max-width: 1100px) {
    .inventory-main {
      grid-template-columns: 1fr;
    }
  }

</style>
