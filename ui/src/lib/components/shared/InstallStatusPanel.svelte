<script lang="ts">
  import StateBadge from '#lib/components/shared/StateBadge.svelte';
  import type { InstallStatusResponse, InstallActionResponse } from '#lib/api/types.ts';

  export let status: InstallStatusResponse | null = null;
  export let loading = false;
  export let actionLoading = false;
  export let error = '';
  export let lastActionResult: InstallActionResponse | null = null;
  export let handleBootstrap: () => Promise<void> | void = () => {};
  export let handleRefresh: () => Promise<void> | void = () => {};
  export let handleRepairBridge: () => Promise<void> | void = () => {};
  export let handleRepairDirectories: () => Promise<void> | void = () => {};
  export let handleRepairLocaldisk: () => Promise<void> | void = () => {};
</script>

<section class="table-card">
  <div class="card-header flex items-center justify-between px-4 py-3">
    <div>
      <div class="text-[11px] uppercase tracking-[0.16em] text-muted">Install Status</div>
      <div class="mt-1 text-base font-semibold">Bootstrap and Host Readiness</div>
    </div>
    {#if status}
      <StateBadge label={status.overall_state} />
    {/if}
  </div>

  <div class="space-y-6 p-4">
    {#if loading}
      <div class="border border-line bg-chrome px-4 py-6 text-sm text-muted">Loading install status…</div>
    {:else if error}
      <div class="border border-danger bg-red-50 px-4 py-4 text-sm text-danger">{error}</div>
    {:else if status}
      <div class="grid gap-6 lg:grid-cols-2">
        <div class="table-card">
          <div class="card-header px-4 py-2 text-sm font-medium">Platform</div>
          <dl class="grid grid-cols-[180px_minmax(0,1fr)] gap-x-4 gap-y-3 p-4 text-sm">
            <dt class="text-muted">Data root</dt>
            <dd class="mono">{status.data_root}</dd>
            <dt class="text-muted">SQLite path</dt>
            <dd class="mono">{status.database_path}</dd>
            <dt class="text-muted">Cloud Hypervisor</dt>
            <dd class="flex items-center gap-3">
              <span class="mono">{status.cloud_hypervisor.path || 'not found'}</span>
              <StateBadge label={status.cloud_hypervisor.found ? 'ready' : 'missing_prerequisites'} />
            </dd>
            <dt class="text-muted">Cloud-init ISO support</dt>
            <dd>
              <StateBadge label={status.cloudinit.supported ? 'ready' : 'missing_prerequisites'} />
            </dd>
          </dl>
        </div>

        <div class="table-card">
          <div class="card-header px-4 py-2 text-sm font-medium">Host Network</div>
          <dl class="grid grid-cols-[180px_minmax(0,1fr)] gap-x-4 gap-y-3 p-4 text-sm">
            <dt class="text-muted">Bridge</dt>
            <dd class="mono">{status.bridge.name}</dd>
            <dt class="text-muted">Exists</dt>
            <dd><StateBadge label={status.bridge.exists ? 'ready' : 'bootstrap_required'} /></dd>
            <dt class="text-muted">Expected IP</dt>
            <dd class="mono">{status.bridge.expected_ip}</dd>
            <dt class="text-muted">Actual IP</dt>
            <dd class="mono">{status.bridge.actual_ip || 'missing'}</dd>
            <dt class="text-muted">Link state</dt>
            <dd><StateBadge label={status.bridge.up ? 'active' : 'degraded'} /></dd>
          </dl>
        </div>
      </div>

      <div class="table-card">
        <div class="card-header px-4 py-2 text-sm font-medium">Storage</div>
        <dl class="grid grid-cols-[180px_minmax(0,1fr)] gap-x-4 gap-y-3 p-4 text-sm">
          <dt class="text-muted">Default pool</dt>
          <dd class="mono">{status.localdisk.path}</dd>
          <dt class="text-muted">Pool state</dt>
          <dd><StateBadge label={status.localdisk.ready ? 'ready' : 'bootstrap_required'} /></dd>
        </dl>
      </div>

      {#if lastActionResult}
        <div class="table-card">
          <div class="card-header px-4 py-2 text-sm font-medium">Last Action Result</div>
          <div class="space-y-3 p-4 text-sm">
            <div class="flex items-center gap-3">
              <span class="text-muted">State:</span>
              <StateBadge label={lastActionResult.overall_state} />
            </div>
            {#if lastActionResult.actions_taken.length > 0}
              <div>
                <span class="text-muted">Actions taken:</span>
                <ul class="mt-1 list-disc pl-5">
                  {#each lastActionResult.actions_taken as action}
                    <li>{action}</li>
                  {/each}
                </ul>
              </div>
            {/if}
            {#if lastActionResult.warnings.length > 0}
              <div class="rounded-sm border border-warning bg-yellow-50 px-3 py-2">
                <span class="text-warning font-medium">Warnings:</span>
                <ul class="mt-1 list-disc pl-5 text-warning">
                  {#each lastActionResult.warnings as warning}
                    <li>{warning}</li>
                  {/each}
                </ul>
              </div>
            {/if}
            {#if lastActionResult.errors.length > 0}
              <div class="rounded-sm border border-danger bg-red-50 px-3 py-2">
                <span class="text-danger font-medium">Errors:</span>
                <ul class="mt-1 list-disc pl-5 text-danger">
                  {#each lastActionResult.errors as err}
                    <li>{err}</li>
                  {/each}
                </ul>
              </div>
            {/if}
          </div>
        </div>
      {/if}

      <div class="flex flex-wrap gap-3">
        <button 
          type="button"
          class="button-primary px-4 py-2 text-sm font-medium" 
          on:click={handleBootstrap}
          disabled={actionLoading}
        >
          {actionLoading ? 'Running…' : 'Bootstrap'}
        </button>
        <button 
          type="button"
          class="button-secondary px-4 py-2 text-sm font-medium" 
          on:click={handleRefresh}
          disabled={actionLoading}
        >
          Re-run Checks
        </button>
        <button 
          type="button"
          class="button-secondary px-4 py-2 text-sm font-medium" 
          on:click={handleRepairBridge}
          disabled={actionLoading}
        >
          Repair Bridge
        </button>
        <button 
          type="button"
          class="button-secondary px-4 py-2 text-sm font-medium" 
          on:click={handleRepairDirectories}
          disabled={actionLoading}
        >
          Repair Directories
        </button>
        <button 
          type="button"
          class="button-secondary px-4 py-2 text-sm font-medium" 
          on:click={handleRepairLocaldisk}
          disabled={actionLoading}
        >
          Repair Localdisk
        </button>
      </div>
    {:else}
      <div class="border border-line bg-chrome px-4 py-6 text-sm text-muted">No install status available yet.</div>
    {/if}
  </div>
</section>
