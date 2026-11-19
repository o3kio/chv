<script lang="ts">
  import Modal from '#lib/components/primitives/Modal.svelte';
  import FormField from '#lib/components/shared/FormField.svelte';
  import Input from '#lib/components/primitives/TextInput.svelte';
  import { createAPIClient, getStoredToken } from '#lib/api/client.ts';
  import { toast } from '#lib/stores/toast.svelte.ts';
  import { mutateWithRefresh } from '#lib/stores/mutation.svelte.ts';
  import { Copy, Check } from 'lucide-svelte';
  import { extractVariables } from './cloudinit-helpers';
  import CloudInitEditorSidebar from './CloudInitEditorSidebar.svelte';

  interface Props {
    open?: boolean;
    initialContent?: string;
    initialName?: string;
    initialDescription?: string;
    onSuccess?: () => void;
  }

  let {
    open = $bindable(false),
    initialContent = '',
    initialName = '',
    initialDescription = '',
    onSuccess
  }: Props = $props();

  const client = createAPIClient({ token: getStoredToken() ?? undefined });

  // Form state
  let name = $state('');
  let description = $state('');
  let content = $state('#cloud-config\n');
  let variables = $state<string[]>([]);

  // Preview state
  let previewVariables = $state<Record<string, string>>({});
  let renderedPreview = $state('');
  let showPreview = $state(false);
  let showVariableHelper = $state(true);

  // UI state
  let copied = $state(false);
  let submitting = $state(false);
  let formError = $state('');
  let nameError = $state('');
  let contentError = $state('');

  const nameRegex = /^[a-zA-Z0-9\s-_]+$/;

  // Extract variables whenever content changes
  $effect(() => {
    variables = extractVariables(content);
  });

  // Reset form when modal opens with new initial values
  $effect(() => {
    if (open) {
      name = initialName;
      description = initialDescription;
      content = initialContent || '#cloud-config\n';
      previewVariables = {};
      renderedPreview = '';
      formError = '';
      nameError = '';
      contentError = '';
    }
  });

  function validateName(): boolean {
    if (!name.trim()) {
      nameError = 'Name is required';
      return false;
    }
    if (!nameRegex.test(name)) {
      nameError = 'Name can only contain letters, numbers, spaces, hyphens, and underscores';
      return false;
    }
    if (name.length > 50) {
      nameError = 'Name must be 50 characters or less';
      return false;
    }
    nameError = '';
    return true;
  }

  function validateContent(): boolean {
    if (!content.trim()) {
      contentError = 'Content is required';
      return false;
    }
    if (!content.includes('#cloud-config')) {
      contentError = 'Content must include #cloud-config header';
      return false;
    }
    contentError = '';
    return true;
  }

  async function updatePreview() {
    if (!validateContent()) {
      renderedPreview = '';
      return;
    }

    try {
      // Create a temporary template to render
      const tempTemplate = {
        content,
        variables
      };

      // Simple client-side rendering for preview
      let rendered = content;
      for (const [key, value] of Object.entries(previewVariables)) {
        const regex = new RegExp(`\\{\\{\\s*\\.${key}\\s*\\}\\}`, 'g');
        rendered = rendered.replace(regex, value);
      }
      renderedPreview = rendered;
    } catch (e) {
      renderedPreview = '# Error rendering preview';
    }
  }

  function copyToClipboard() {
    navigator.clipboard.writeText(content).then(() => {
      copied = true;
      setTimeout(() => copied = false, 2000);
      toast.success('Copied to clipboard');
    });
  }

  function insertVariable(varName: string) {
    const textarea = document.getElementById('cloudinit-content') as HTMLTextAreaElement;
    if (!textarea) return;

    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const before = content.substring(0, start);
    const after = content.substring(end);
    const insertion = `{{.${varName}}}`;

    content = before + insertion + after;

    // Set cursor position after insertion
    setTimeout(() => {
      textarea.selectionStart = textarea.selectionEnd = start + insertion.length;
      textarea.focus();
    }, 0);
  }

  function insertSnippet(snippet: string) {
    const textarea = document.getElementById('cloudinit-content') as HTMLTextAreaElement;
    if (!textarea) return;

    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const before = content.substring(0, start);
    const after = content.substring(end);

    content = before + snippet + after;

    // Set cursor position after insertion
    setTimeout(() => {
      textarea.selectionStart = textarea.selectionEnd = start + snippet.length;
      textarea.focus();
    }, 0);
  }

  async function handleSubmit() {
    const isNameValid = validateName();
    const isContentValid = validateContent();

    if (!isNameValid || !isContentValid) {
      return;
    }

    submitting = true;
    formError = '';

    try {
      await mutateWithRefresh(
        () => client.createCloudInitTemplate({
          name: name.trim(),
          description: description.trim() || undefined,
          content: content.trim()
        }),
        {
          successMessage: 'Template created successfully',
          errorMessage: 'Failed to create template',
        }
      );

      open = false;
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to create template';
      formError = message;
    } finally {
      submitting = false;
    }
  }
</script>

<Modal bind:open title={initialName ? 'Edit Cloud-init Template' : 'Create Cloud-init Template'} closeOnBackdrop={!submitting} width="wide">
  <div class="space-y-5">
    {#if formError}
      <div class="rounded-sm border border-danger/30 bg-danger/10 px-3 py-2 text-sm text-danger" role="alert">
        {formError}
      </div>
    {/if}

    <!-- Name -->
    <FormField label="Template Name" error={nameError} required labelFor="template-name">
      <Input
        id="template-name"
        bind:value={name}
        placeholder="e.g., My Custom Template"
        disabled={submitting || !!initialName}
        onblur={validateName}
      />
    </FormField>

    <!-- Description -->
    <FormField label="Description" labelFor="template-description">
      <Input
        id="template-description"
        bind:value={description}
        placeholder="Brief description of what this template does..."
        disabled={submitting}
      />
    </FormField>

    <div class="grid grid-cols-1 lg:grid-cols-2 gap-4">
      <!-- Editor -->
      <div>
        <div class="flex items-center justify-between mb-2">
          <label for="cloudinit-content" class="text-sm font-medium text-ink">
            Cloud-init Content
          </label>
          <div class="flex items-center gap-2">
            <button
              type="button"
              onclick={copyToClipboard}
              class="flex items-center gap-1 text-xs text-muted hover:text-ink"
              title="Copy to clipboard"
            >
              {#if copied}
                <Check size={12} class="text-success" />
              {:else}
                <Copy size={12} />
              {/if}
              {copied ? 'Copied!' : 'Copy'}
            </button>
          </div>
        </div>

        {#if contentError}
          <div class="text-xs text-danger mb-1">{contentError}</div>
        {/if}

        <textarea
          id="cloudinit-content"
          bind:value={content}
          class="w-full rounded-sm border border-[#CCCCCC] bg-white px-3 py-2 font-mono text-sm focus:border-primary focus:outline-hidden focus:ring-2 focus:ring-primary/20"
          rows={20}
          disabled={submitting}
          spellcheck={false}
        ></textarea>

        <div class="mt-2 text-xs text-muted">
          {content.length} characters • {variables.length} variable{variables.length !== 1 ? 's' : ''}
        </div>
      </div>

      <!-- Sidebar: Variables & Snippets -->
      <CloudInitEditorSidebar
        {variables}
        bind:previewVariables
        bind:showPreview
        {renderedPreview}
        {insertVariable}
        {insertSnippet}
        {updatePreview}
      />
    </div>
  </div>

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
      disabled={submitting || !name.trim() || !content.trim()}
      class="px-4 py-2 rounded-sm bg-primary text-white font-medium hover:bg-primary/90 transition-colors disabled:bg-primary/30 disabled:cursor-not-allowed flex items-center gap-2"
    >
      {#if submitting}
        <svg class="animate-spin h-4 w-4" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" aria-hidden="true">
          <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
          <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
        </svg>
      {/if}
      {submitting ? 'Creating...' : (initialName ? 'Update Template' : 'Create Template')}
    </button>
  {/snippet}
</Modal>
