<script lang="ts">
  import { Plus, Copy, Check, Server, ExternalLink } from 'lucide-svelte';
  import Modal from '../primitives/Modal.svelte';
  import Input from '../primitives/Input.svelte';
  import FormField from '../shared/FormField.svelte';
  import type { CreateNodeInput, CreateNodeResponse } from '#lib/api/types.ts';

  interface Props {
    open: boolean;
    onClose: () => void;
    onSubmit: (data: CreateNodeInput) => Promise<CreateNodeResponse>;
  }

  let { open = $bindable(false), onClose, onSubmit }: Props = $props();

  let name = $state('');
  let hostname = $state('');
  let ipAddress = $state('');
  let agentUrl = $state('');
  let loading = $state(false);
  let error = $state<string | null>(null);
  let result = $state<CreateNodeResponse | null>(null);
  let copied = $state(false);

  function resetForm() {
    name = '';
    hostname = '';
    ipAddress = '';
    agentUrl = '';
    error = null;
    result = null;
    copied = false;
  }

  function handleClose() {
    resetForm();
    onClose();
  }

  async function handleSubmit(e: Event) {
    e.preventDefault();
    error = null;
    loading = true;

    try {
      // Validation
      if (!name.trim()) {
        error = 'Node name is required';
        return;
      }
      if (!hostname.trim()) {
        error = 'Hostname is required';
        return;
      }
      if (!ipAddress.trim()) {
        error = 'IP address is required';
        return;
      }

      // Basic IP validation
      const ipRegex = /^(\d{1,3}\.){3}\d{1,3}$/;
      if (!ipRegex.test(ipAddress)) {
        error = 'Please enter a valid IP address (e.g., 10.0.0.5)';
        return;
      }

      const data: CreateNodeInput = {
        name: name.trim(),
        hostname: hostname.trim(),
        ip_address: ipAddress.trim(),
        agent_url: agentUrl.trim() || undefined
      };

      result = await onSubmit(data);
    } catch (err: any) {
      error = err instanceof Error ? err.message : 'Failed to create node';
    } finally {
      loading = false;
    }
  }

  async function copyToken() {
    if (result?.agent_token) {
      try {
        await navigator.clipboard.writeText(result.agent_token);
        copied = true;
        setTimeout(() => copied = false, 2000);
      } catch {
        // Clipboard API not available, ignore
      }
    }
  }

  function copyAgentInstallCommand() {
    if (result) {
      const controllerUrl = typeof window !== 'undefined' ? window.location.origin : '';
      const command = `# Install CHV Agent on ${result.hostname}
# 1. Download and install the agent
curl -fsSL ${controllerUrl}/install-agent.sh | sudo bash

# 2. Configure the agent with the token below
sudo chv-agent configure \\
  --controller ${controllerUrl} \\
  --node-id ${result.id} \\
  --token ${result.agent_token}

# 3. Start the agent
sudo systemctl enable --now chv-agent
`;
      try {
        navigator.clipboard.writeText(command);
      } catch {
        // Ignore
      }
    }
  }
</script>

<Modal
  {open}
  onClose={handleClose}
  title={result ? 'Node Created Successfully' : 'Add New Node'}
  description={result ? 'Save the agent token - it will only be shown once' : 'Register a new remote hypervisor node'}
  width="wide"
>
  {#if result}
    <!-- Success State - Show Token -->
    <div class="space-y-6">
      <div class="bg-green-50 border border-green-200 rounded-lg p-4">
        <div class="flex items-start gap-3">
          <div class="w-10 h-10 rounded-full bg-green-100 flex items-center justify-center shrink-0">
            <Server class="text-green-600" size={20} />
          </div>
          <div>
            <h4 class="font-medium text-green-900">{result.name}</h4>
            <p class="text-sm text-green-700 mt-1">{result.hostname} ({result.ip_address})</p>
          </div>
        </div>
      </div>

      <div class="space-y-2">
        <p class="block text-sm font-medium text-slate-700">
          Agent Token <span class="text-red-500">*</span>
          <span class="text-xs font-normal text-slate-500 ml-2">Copy and save this token - it will not be shown again</span>
        </p>
        <div class="flex gap-2">
          <code class="flex-1 bg-slate-900 text-green-400 px-4 py-3 rounded-lg text-sm font-mono break-all">
            {result.agent_token}
          </code>
          <button
            type="button"
            onclick={copyToken}
            class="px-4 py-2 bg-white border border-slate-200 rounded-lg hover:bg-slate-50 transition-colors flex items-center gap-2"
            aria-label="Copy token to clipboard"
          >
            {#if copied}
              <Check size={18} class="text-green-600" />
              <span class="text-sm text-green-600">Copied!</span>
            {:else}
              <Copy size={18} class="text-slate-600" />
              <span class="text-sm text-slate-600">Copy</span>
            {/if}
          </button>
        </div>
      </div>

      {#if result.agent_url}
        <div class="space-y-2">
          <p class="block text-sm font-medium text-slate-700">Agent URL</p>
          <div class="flex gap-2">
            <code class="flex-1 bg-slate-100 text-slate-700 px-4 py-3 rounded-lg text-sm font-mono">
              {result.agent_url}
            </code>
          </div>
        </div>
      {/if}

      <div class="bg-amber-50 border border-amber-200 rounded-lg p-4">
        <h5 class="font-medium text-amber-900 mb-2 flex items-center gap-2">
          <ExternalLink size={16} />
          Next Steps
        </h5>
        <ol class="text-sm text-amber-800 space-y-2 ml-4 list-decimal">
          <li>Copy and save the agent token above securely</li>
          <li>Install the CHV agent on <strong>{result.hostname}</strong></li>
          <li>Configure the agent with the node ID and token</li>
          <li>The agent will automatically register and connect</li>
        </ol>
      </div>

      <div class="flex justify-end gap-3 pt-4 border-t border-slate-200">
        <button
          type="button"
          onclick={handleClose}
          class="px-4 py-2 bg-orange-500 text-white rounded-lg hover:bg-orange-600 transition-colors font-medium"
        >
          Done
        </button>
      </div>
    </div>
  {:else}
    <!-- Form State -->
    <form onsubmit={handleSubmit} class="space-y-5">
      {#if error}
        <div class="bg-red-50 border border-red-200 rounded-lg p-4">
          <p class="text-sm text-red-700">{error}</p>
        </div>
      {/if}

      <FormField label="Node Name" required error={error && !name ? 'Name is required' : undefined}>
        <Input
          type="text"
          bind:value={name}
          placeholder="e.g., hypervisor-02"
          disabled={loading}
          autocomplete="off"
        />
      </FormField>

      <FormField label="Hostname" required error={error && !hostname ? 'Hostname is required' : undefined}>
        <Input
          type="text"
          bind:value={hostname}
          placeholder="e.g., hv02.example.com"
          disabled={loading}
          autocomplete="off"
        />
      </FormField>

      <FormField label="IP Address" required error={error && !ipAddress ? 'IP address is required' : undefined}>
        <Input
          type="text"
          bind:value={ipAddress}
          placeholder="e.g., 10.0.1.10"
          disabled={loading}
          autocomplete="off"
        />
      </FormField>

      <FormField label="Agent URL" helper="Optional URL where the agent will be accessible">
        <Input
          type="url"
          bind:value={agentUrl}
          placeholder="e.g., http://10.0.1.10:9090"
          disabled={loading}
          autocomplete="off"
        />
      </FormField>

      <div class="flex justify-end gap-3 pt-4 border-t border-slate-200">
        <button
          type="button"
          onclick={handleClose}
          class="px-4 py-2 text-slate-700 hover:bg-slate-100 rounded-lg transition-colors font-medium"
          disabled={loading}
        >
          Cancel
        </button>
        <button
          type="submit"
          class="px-4 py-2 bg-orange-500 text-white rounded-lg hover:bg-orange-600 transition-colors font-medium flex items-center gap-2 disabled:opacity-50"
          disabled={loading}
        >
          {#if loading}
            <div class="w-4 h-4 border-2 border-white/30 border-t-white rounded-full animate-spin"></div>
            <span>Creating...</span>
          {:else}
            <Plus size={18} />
            <span>Add Node</span>
          {/if}
        </button>
      </div>
    </form>
  {/if}
</Modal>
