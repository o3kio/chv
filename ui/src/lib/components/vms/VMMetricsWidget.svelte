<script>
  import { Activity, Cpu, MemoryStick, HardDrive } from 'lucide-svelte';
  
  // Props
  let { vms = { total: 0, running: 0, stopped: 0, error: 0 } } = $props();
  
  // Calculate percentages for visualization
  let runningPercent = $derived(vms.total > 0 ? (vms.running / vms.total) * 100 : 0);
  let stoppedPercent = $derived(vms.total > 0 ? (vms.stopped / vms.total) * 100 : 0);
  let errorPercent = $derived(vms.total > 0 ? (vms.error / vms.total) * 100 : 0);
</script>

<div class="bg-(--shell-surface) rounded-lg border border-(--shell-line) p-6">
  <h2 class="text-lg font-semibold text-(--shell-text) mb-4 flex items-center gap-2">
    <Activity size={20} class="text-(--shell-text-muted)" />
    VM Resource Overview
  </h2>
  
  <div class="space-y-6">
    <!-- VM State Distribution -->
    <div>
      <h3 class="text-sm font-medium text-(--shell-text-secondary) mb-3">VM State Distribution</h3>
      
      <!-- Progress Bar -->
      <div class="h-4 bg-(--shell-surface-muted) rounded-full overflow-hidden flex">
        {#if runningPercent > 0}
          <div 
            class="h-full bg-success transition-all duration-500"
            style="width: {runningPercent}%"
            title="Running: {vms.running}"
          ></div>
        {/if}
        {#if stoppedPercent > 0}
          <div 
            class="h-full bg-(--shell-text-muted) transition-all duration-500"
            style="width: {stoppedPercent}%"
            title="Stopped: {vms.stopped}"
          ></div>
        {/if}
        {#if errorPercent > 0}
          <div 
            class="h-full bg-danger transition-all duration-500"
            style="width: {errorPercent}%"
            title="Error: {vms.error}"
          ></div>
        {/if}
      </div>
      
      <!-- Legend -->
      <div class="flex flex-wrap gap-4 mt-3">
        <div class="flex items-center gap-2">
          <div class="w-3 h-3 rounded-full bg-success"></div>
          <span class="text-sm text-(--shell-text-secondary)">Running ({vms.running})</span>
        </div>
        <div class="flex items-center gap-2">
          <div class="w-3 h-3 rounded-full bg-(--shell-text-muted)"></div>
          <span class="text-sm text-(--shell-text-secondary)">Stopped ({vms.stopped})</span>
        </div>
        {#if vms.error > 0}
          <div class="flex items-center gap-2">
            <div class="w-3 h-3 rounded-full bg-danger"></div>
            <span class="text-sm text-(--shell-text-secondary)">Error ({vms.error})</span>
          </div>
        {/if}
      </div>
    </div>
    
    <!-- Resource Metrics Grid -->
    <div class="grid grid-cols-2 gap-4">
      <div class="bg-(--shell-surface-muted) rounded-lg p-4">
        <div class="flex items-center gap-2 mb-2">
          <Cpu size={18} class="text-(--shell-text-muted)" />
          <span class="text-sm font-medium text-(--shell-text-secondary)">CPU Usage</span>
        </div>
        <div class="text-2xl font-semibold text-(--shell-text)">
          {vms.running > 0 ? 'Active' : '—'}
        </div>
        <div class="text-xs text-(--shell-text-muted) mt-1">
          {#if vms.running > 0}
            {vms.running} VMs using CPU
          {:else}
            No running VMs
          {/if}
        </div>
      </div>
      
      <div class="bg-(--shell-surface-muted) rounded-lg p-4">
        <div class="flex items-center gap-2 mb-2">
          <MemoryStick size={18} class="text-(--shell-text-muted)" />
          <span class="text-sm font-medium text-(--shell-text-secondary)">Memory</span>
        </div>
        <div class="text-2xl font-semibold text-(--shell-text)">
          {vms.running > 0 ? 'Allocated' : '—'}
        </div>
        <div class="text-xs text-(--shell-text-muted) mt-1">
          {#if vms.running > 0}
            Memory allocated to VMs
          {:else}
            No memory in use
          {/if}
        </div>
      </div>
      
      <div class="bg-(--shell-surface-muted) rounded-lg p-4">
        <div class="flex items-center gap-2 mb-2">
          <HardDrive size={18} class="text-warning" />
          <span class="text-sm font-medium text-(--shell-text-secondary)">Storage</span>
        </div>
        <div class="text-2xl font-semibold text-(--shell-text)">
          {vms.total > 0 ? 'Provisioned' : '—'}
        </div>
        <div class="text-xs text-(--shell-text-muted) mt-1">
          {#if vms.total > 0}
            {vms.total} VM disks
          {:else}
            No VMs created
          {/if}
        </div>
      </div>
      
      <div class="bg-(--shell-surface-muted) rounded-lg p-4">
        <div class="flex items-center gap-2 mb-2">
          <Activity size={18} class="text-success" />
          <span class="text-sm font-medium text-(--shell-text-secondary)">Health</span>
        </div>
        <div class="text-2xl font-semibold text-(--shell-text)">
          {#if vms.error > 0}
            <span class="text-danger">{vms.error} Issues</span>
          {:else if vms.running > 0}
            <span class="text-success">Healthy</span>
          {:else}
            <span class="text-(--shell-text-muted)">Idle</span>
          {/if}
        </div>
        <div class="text-xs text-(--shell-text-muted) mt-1">
          {#if vms.error > 0}
            {vms.error} VM(s) in error state
          {:else if vms.running > 0}
            All systems operational
          {:else}
            No active VMs
          {/if}
        </div>
      </div>
    </div>
    
    <!-- Quick Stats -->
    <div class="border-t border-(--shell-line) pt-4">
      <h3 class="text-sm font-medium text-(--shell-text-secondary) mb-3">Quick Stats</h3>
      <div class="grid grid-cols-3 gap-4 text-center">
        <div class="p-3 bg-(--color-success-light) rounded-lg">
          <div class="text-2xl font-semibold text-success">{vms.running}</div>
          <div class="text-xs text-(--color-success-dark) mt-1">Active</div>
        </div>
        <div class="p-3 bg-(--shell-surface-muted) rounded-lg">
          <div class="text-2xl font-semibold text-(--shell-text-secondary)">{vms.stopped}</div>
          <div class="text-xs text-(--shell-text-secondary) mt-1">Inactive</div>
        </div>
        <div class="p-3 bg-(--shell-surface-muted) rounded-lg">
          <div class="text-2xl font-semibold text-(--shell-text)">{vms.total}</div>
          <div class="text-xs text-(--shell-text-secondary) mt-1">Total</div>
        </div>
      </div>
    </div>
  </div>
</div>
