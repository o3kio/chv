<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { page } from '$app/state';
  import { createAPIClient } from '#lib/api/client.ts';
  import { 
    LayoutDashboard, 
    Wrench, 
    Network, 
    HardDrive, 
    Image as ImageIcon, 
    Cpu, 
    Activity, 
    Bell, 
    Settings,
    ChevronRight,
    Box
  } from 'lucide-svelte';

  export let currentPath = '/';

  const items = [
    { href: '/', label: 'Overview', icon: LayoutDashboard },
    { href: '/install', label: 'Install', icon: Wrench },
    { href: '/networks', label: 'Networks', icon: Network },
    { href: '/volumes', label: 'Storage', icon: HardDrive },
    { href: '/images', label: 'Images', icon: ImageIcon },
    { href: '/vms', label: 'Virtual Machines', icon: Cpu },
    { href: '/templates', label: 'Templates', icon: Box },
    { href: '/operations', label: 'Operations', icon: Activity },
    { href: '/events', label: 'Events', icon: Bell },
    { href: '/settings', label: 'Settings', icon: Settings }
  ];

  let client: ReturnType<typeof createAPIClient>;
  let newEvents = 0;
  let lastEventCheck = new Date();
  let pollInterval: ReturnType<typeof setInterval>;

  onMount(() => {
    client = createAPIClient();
    // Check immediately
    checkNewEvents();

    // Poll every 30 seconds
    pollInterval = setInterval(checkNewEvents, 30000);

    return () => {
      if (pollInterval) clearInterval(pollInterval);
    };
  });

  onDestroy(() => {
    if (pollInterval) clearInterval(pollInterval);
  });

  async function checkNewEvents() {
    if (!client) return;
    try {
      const events = (await client.listEvents()) ?? [];
      // Count events newer than last check
      newEvents = events.filter(e => new Date(e.timestamp) > lastEventCheck).length;
    } catch (err) {
      console.error('Failed to check events:', err);
    }
  }

  function clearBadge() {
    newEvents = 0;
    lastEventCheck = new Date();
  }

  // Clear badge when navigating to events page
  $: if (page?.url?.pathname === '/events') {
    clearBadge();
  }
</script>

<aside class="sidebar h-screen flex flex-col bg-[var(--bg-sidebar)] text-[var(--color-neutral-400)]">
  <div class="px-6 py-8">
    <div class="flex items-center gap-3">
      <div class="bg-[var(--color-primary)] p-2 rounded-lg shadow-lg shadow-[var(--color-primary)]/20">
        <Cpu class="text-white" size={20} />
      </div>
      <div>
        <div class="text-[10px] font-bold uppercase tracking-[0.2em] text-[var(--color-primary-light)] leading-none">Antigravity</div>
        <div class="mt-1 text-lg font-bold text-white tracking-tight">CHV Operator</div>
      </div>
    </div>
  </div>

  <nav class="flex-1 px-3 space-y-1 overflow-y-auto custom-scrollbar">
    {#each items as item}
      {@const isActive = currentPath === item.href}
      <a
        class={`group relative flex items-center gap-3 px-3 py-2.5 rounded-lg text-sm font-medium transition-all duration-200 ${
          isActive
            ? 'bg-[var(--color-primary)]/10 text-white shadow-sm'
            : 'hover:bg-[var(--color-neutral-800)]/50 hover:text-[var(--color-neutral-100)]'
        }`}
        href={item.href}
        on:click={() => item.href === '/events' && clearBadge()}
      >
        {#if isActive}
          <div class="absolute left-0 w-1 h-5 bg-[var(--color-primary)] rounded-r-full"></div>
        {/if}
        
        <item.icon 
          size={18} 
          class={`transition-colors duration-200 ${isActive ? 'text-[var(--color-primary)]' : 'text-[var(--color-neutral-500)] group-hover:text-[var(--color-neutral-300)]'}`} 
        />
        
        <span class="flex-1">{item.label}</span>
        
        {#if item.href === '/events' && newEvents > 0}
          <span class="badge absolute right-3 shrink-0">{newEvents}</span>
        {:else if isActive}
          <ChevronRight size={14} class="text-[var(--color-primary)]/50" />
        {/if}
      </a>
    {/each}
  </nav>

  <div class="p-4 border-t border-[var(--color-neutral-800)]">
    <div class="flex items-center gap-3 px-3 py-2">
      <div class="w-8 h-8 rounded-full bg-[var(--color-neutral-800)] border border-[var(--color-neutral-700)] flex items-center justify-center text-[10px] font-bold text-[var(--color-neutral-300)]">
        AD
      </div>
      <div class="min-w-0">
        <div class="text-xs font-semibold text-white truncate">Administrator</div>
        <div class="text-[10px] text-[var(--color-neutral-500)] truncate">chv-v0.3.0-alpha</div>
      </div>
    </div>
  </div>
</aside>

<style>
  .badge {
    background: linear-gradient(135deg, var(--color-danger), var(--color-danger-dark));
    color: white;
    border-radius: 6px;
    padding: 2px 6px;
    font-size: 10px;
    font-weight: 700;
    min-width: 18px;
    height: 18px;
    display: flex;
    align-items: center;
    justify-content: center;
    box-shadow: 0 2px 8px rgba(239, 68, 68, 0.4);
  }

  /* Custom scrollbar for pure UI feel */
  .custom-scrollbar::-webkit-scrollbar {
    width: 4px;
  }
  .custom-scrollbar::-webkit-scrollbar-track {
    background: transparent;
  }
  .custom-scrollbar::-webkit-scrollbar-thumb {
    background: var(--color-neutral-700);
    border-radius: 10px;
  }
  .custom-scrollbar::-webkit-scrollbar-thumb:hover {
    background: var(--color-neutral-600);
  }
</style>
