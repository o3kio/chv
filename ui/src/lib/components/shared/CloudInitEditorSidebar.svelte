<script lang="ts">
  import { Eye, EyeOff, Variable, FileCode } from 'lucide-svelte';
  import { COMMON_SNIPPETS, highlightYAML } from './cloudinit-helpers';

  interface Props {
    variables: string[];
    previewVariables: Record<string, string>;
    showPreview: boolean;
    renderedPreview: string;
    insertVariable: (varName: string) => void;
    insertSnippet: (snippet: string) => void;
    updatePreview: () => void;
  }

  let {
    variables,
    previewVariables = $bindable(),
    showPreview = $bindable(),
    renderedPreview,
    insertVariable,
    insertSnippet,
    updatePreview
  }: Props = $props();
</script>

<div class="space-y-4">
  <!-- Variables Section -->
  <div class="bg-chrome rounded-lg p-4">
    <div class="flex items-center gap-2 mb-3">
      <Variable size={16} class="text-primary" />
      <h4 class="text-sm font-semibold text-ink">Template Variables</h4>
    </div>

    {#if variables.length > 0}
      <div class="flex flex-wrap gap-2 mb-3">
        {#each variables as varName}
          <button
            type="button"
            onclick={() => insertVariable(varName)}
            class="text-xs bg-white px-2 py-1 rounded-sm border border-line text-primary hover:border-primary transition-colors"
            title="Insert variable"
          >
            {'{{.' + varName + '}}'}
          </button>
        {/each}
      </div>

      <!-- Preview Inputs -->
      <div class="border-t border-line pt-3 mt-3">
        <h5 class="text-xs font-medium text-muted mb-2">Preview Values</h5>
        <div class="space-y-2">
          {#each variables as varName}
            <div>
              <label for={`preview-${varName}`} class="block text-xs text-muted mb-1">{varName}</label>
              <input
                id={`preview-${varName}`}
                type="text"
                value={previewVariables[varName] || ''}
                oninput={(e) => {
                  previewVariables = { ...previewVariables, [varName]: e.currentTarget.value };
                }}
                placeholder={`Enter ${varName}...`}
                class="w-full h-7 rounded-sm border border-[#CCCCCC] bg-white px-2 py-1 text-sm"
              />
            </div>
          {/each}
        </div>
        <button
          type="button"
          onclick={() => { showPreview = true; updatePreview(); }}
          class="mt-3 text-xs text-primary hover:text-primary/80 font-medium"
        >
          Update Preview
        </button>
      </div>
    {:else}
      <p class="text-sm text-muted">
        No variables detected. Use {'{{.VariableName}}'} syntax to add variables.
      </p>
    {/if}
  </div>

  <!-- Snippets Section -->
  <div class="bg-chrome rounded-lg p-4">
    <div class="flex items-center gap-2 mb-3">
      <FileCode size={16} class="text-primary" />
      <h4 class="text-sm font-semibold text-ink">Quick Snippets</h4>
    </div>
    <div class="space-y-2">
      {#each COMMON_SNIPPETS as snippet}
        <button
          type="button"
          onclick={() => insertSnippet(snippet.snippet)}
          class="w-full text-left text-xs px-3 py-2 rounded-sm border border-line hover:border-primary hover:bg-white transition-colors"
        >
          {snippet.name}
        </button>
      {/each}
    </div>
  </div>

  <!-- Preview Section -->
  {#if showPreview && renderedPreview}
    <div class="bg-chrome rounded-lg p-4">
      <div class="flex items-center justify-between mb-2">
        <h4 class="text-sm font-semibold text-ink">Rendered Preview</h4>
        <button
          type="button"
          onclick={() => showPreview = false}
          class="text-xs text-muted hover:text-ink"
          aria-label="Hide preview"
          title="Hide preview"
        >
          <EyeOff size={12} />
        </button>
      </div>
      <div class="rounded-sm bg-neutral-900 overflow-auto max-h-64">
        <pre class="p-3 text-xs font-mono whitespace-pre-wrap"><code>{@html highlightYAML(renderedPreview)}</code></pre>
      </div>
    </div>
  {:else}
    <button
      type="button"
      onclick={() => { showPreview = true; updatePreview(); }}
      class="flex items-center gap-2 text-sm text-primary hover:text-primary/80"
    >
      <Eye size={16} />
      Show Preview
    </button>
  {/if}
</div>
