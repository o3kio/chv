<script lang="ts">
  import { 
    Activity, 
    Server, 
    Image as ImageIcon, 
    Network, 
    HardDrive, 
    ChevronDown, 
    ChevronUp, 
    Trash2,
    CheckCheck,
    Loader2,
    Filter,
    X
  } from 'lucide-svelte';
  import StateBadge from '../shared/StateBadge.svelte';
  import type { Event } from '#lib/api/types.ts';

  interface Props {
    events: Event[];
    loading?: boolean;
    onFilter?: (filter: string | null) => void;
    onClear?: () => void;
    onMarkAllRead?: () => void;
  }

  let { 
    events, 
    loading = false, 
    onFilter, 
    onClear, 
    onMarkAllRead 
  }: Props = $props();

  let expandedEvents = $state<Set<string>>(new Set());
  let currentFilter = $state<string | null>(null);
  let currentPage = $state(1);
  const itemsPerPage = 10;

  const resourceIcons: Record<string, typeof Server> = {
    vm: Server,
    image: ImageIcon,
    network: Network,
    storage: HardDrive,
    default: Activity
  };

  const resourceFilters = [
    { id: 'vm', label: 'VM', icon: Server },
    { id: 'image', label: 'Image', icon: ImageIcon },
    { id: 'network', label: 'Network', icon: Network },
    { id: 'storage', label: 'Storage', icon: HardDrive }
  ];

  // Functions instead of derived to avoid array creation
  function getFilteredEvents() {
    const filter = currentFilter;
    return filter 
      ? events.filter(e => e.resource.toLowerCase().includes(filter.toLowerCase())) 
      : events;
  }

  function getPaginatedEvents() {
    const filtered = getFilteredEvents();
    const start = (currentPage - 1) * itemsPerPage;
    return filtered.slice(start, start + itemsPerPage);
  }

  function getTotalPages() {
    return Math.ceil(getFilteredEvents().length / itemsPerPage);
  }

  function toggleExpand(id: string) {
    const newSet = new Set(expandedEvents);
    if (newSet.has(id)) {
      newSet.delete(id);
    } else {
      newSet.add(id);
    }
    expandedEvents = newSet;
  }

  function setFilter(filter: string | null) {
    currentFilter = filter;
    currentPage = 1;
    onFilter?.(filter);
  }

  function formatRelativeTime(timestamp: string): string {
    const date = new Date(timestamp);
    const now = new Date();
    const diffMs = now.getTime() - date.getTime();
    const diffSec = Math.floor(diffMs / 1000);
    const diffMin = Math.floor(diffSec / 60);
    const diffHour = Math.floor(diffMin / 60);
    const diffDay = Math.floor(diffHour / 24);

    if (diffSec < 60) return 'just now';
    if (diffMin < 60) return `${diffMin}m ago`;
    if (diffHour < 24) return `${diffHour}h ago`;
    if (diffDay < 7) return `${diffDay}d ago`;
    return date.toLocaleDateString();
  }

  function formatFullTime(timestamp: string): string {
    return new Date(timestamp).toLocaleString();
  }

  function getResourceIcon(resource: string): typeof Server {
    const key = resource.toLowerCase();
    return resourceIcons[key] || resourceIcons.default;
  }

  function goToPage(page: number) {
    if (page >= 1 && page <= getTotalPages()) {
      currentPage = page;
    }
  }
</script>

<div class="event-list card" role="region" aria-label="Events List">
  <!-- Header -->
  <div class="px-5 py-4 border-b border-slate-100 bg-slate-50/50">
    <div class="flex flex-col sm:flex-row sm:items-center justify-between gap-4">
      <div>
        <h3 class="font-semibold text-slate-900">Recent Events</h3>
        <p class="text-sm text-slate-500 mt-0.5">
          {#if loading}
            Loading events...
          {:else}
            {getFilteredEvents().length} events
            {#if currentFilter}
              <span class="text-slate-400">(filtered)</span>
            {/if}
          {/if}
        </p>
      </div>
      
      <div class="flex items-center gap-2">
        <!-- Filter Dropdown -->
        <div class="relative">
          <select
            value={currentFilter || ''}
            onchange={(e) => setFilter((e.target as HTMLSelectElement).value || null)}
            class="appearance-none bg-white border border-slate-200 text-slate-700 text-sm rounded-lg px-3 py-2 pr-8 focus:outline-hidden focus:ring-2 focus:ring-orange-500/20 focus:border-orange-500"
            aria-label="Filter by resource type"
          >
            <option value="">All Types</option>
            {#each resourceFilters as filter}
              <option value={filter.id}>{filter.label}</option>
            {/each}
          </select>
          <Filter size={14} class="absolute right-2.5 top-1/2 -translate-y-1/2 text-slate-400 pointer-events-none" />
        </div>

        <!-- Actions -->
        {#if onMarkAllRead}
          <button
            type="button"
            onclick={onMarkAllRead}
            class="p-2 text-slate-500 hover:text-slate-700 hover:bg-slate-100 rounded-lg transition-colors"
            title="Mark all as read"
            aria-label="Mark all as read"
          >
            <CheckCheck size={16} />
          </button>
        {/if}
        
        {#if onClear && events.length > 0}
          <button
            type="button"
            onclick={onClear}
            class="p-2 text-slate-500 hover:text-red-600 hover:bg-red-50 rounded-lg transition-colors"
            title="Clear all"
            aria-label="Clear all events"
          >
            <Trash2 size={16} />
          </button>
        {/if}
      </div>
    </div>

    <!-- Active Filter Tag -->
    {#if currentFilter}
      <div class="flex items-center gap-2 mt-3">
        <span class="inline-flex items-center gap-1.5 px-2.5 py-1 bg-orange-50 text-orange-700 text-xs font-medium rounded-full border border-orange-200">
          {resourceFilters.find(f => f.id === currentFilter)?.label || currentFilter}
          <button
            type="button"
            onclick={() => setFilter(null)}
            class="hover:text-orange-900"
            aria-label="Clear filter"
          >
            <X size={12} />
          </button>
        </span>
      </div>
    {/if}
  </div>

  <!-- Events List -->
  <div class="divide-y divide-slate-100">
    {#if loading}
      <div class="p-8 text-center">
        <Loader2 size={24} class="mx-auto mb-2 text-slate-400 animate-spin" />
        <p class="text-sm text-slate-500">Loading events...</p>
      </div>
    {:else if getPaginatedEvents().length === 0}
      <div class="p-8 text-center">
        <Activity size={32} class="mx-auto mb-3 opacity-40 text-slate-400" />
        <p class="text-sm text-slate-500">
          {currentFilter ? 'No events match the filter' : 'No recent events'}
        </p>
      </div>
    {:else}
      {#each getPaginatedEvents() as event}
        {@const ResourceIcon = getResourceIcon(event.resource)}
        {@const isExpanded = expandedEvents.has(event.id)}
        <div class="event-item">
          <button
            id="event-trigger-{event.id}"
            type="button"
            class="w-full px-5 py-3 flex items-center justify-between hover:bg-slate-50 transition-colors text-left"
            onclick={() => toggleExpand(event.id)}
            aria-expanded={isExpanded}
            aria-controls="event-content-{event.id}"
          >
            <div class="flex items-center gap-3 min-w-0 flex-1">
              <div class="p-1.5 bg-slate-100 rounded-md shrink-0">
                <ResourceIcon size={14} class="text-slate-500" />
              </div>
              <StateBadge label={event.status} />
              <span class="text-sm font-medium text-slate-700 capitalize truncate">
                {event.operation}
              </span>
              <span class="text-xs text-slate-400 hidden sm:inline">
                {event.resource}
                {#if event.resource_id}
                  <span class="font-mono">({event.resource_id.slice(0, 8)}...)</span>
                {/if}
              </span>
            </div>
            <div class="flex items-center gap-2 shrink-0">
              <span class="text-xs text-slate-500 whitespace-nowrap" title={formatFullTime(event.timestamp)}>
                {formatRelativeTime(event.timestamp)}
              </span>
              <ChevronDown 
                size={14} 
                class="text-slate-400 transition-transform {isExpanded ? 'rotate-180' : ''}" 
              />
            </div>
          </button>
          
          {#if isExpanded}
            <div
              id="event-content-{event.id}"
              class="px-5 pb-3 pl-14 bg-slate-50/50"
              role="region"
              aria-labelledby="event-trigger-{event.id}"
            >
              <div class="space-y-2">
                {#if event.message}
                  <p class="text-sm text-slate-600">{event.message}</p>
                {/if}
                <div class="grid grid-cols-2 gap-4 text-xs">
                  <div>
                    <span class="text-slate-400">Resource:</span>
                    <span class="text-slate-600 ml-1 capitalize">{event.resource}</span>
                  </div>
                  {#if event.resource_id}
                    <div>
                      <span class="text-slate-400">ID:</span>
                      <span class="text-slate-600 ml-1 font-mono">{event.resource_id}</span>
                    </div>
                  {/if}
                  <div>
                    <span class="text-slate-400">Time:</span>
                    <span class="text-slate-600 ml-1">{formatFullTime(event.timestamp)}</span>
                  </div>
                  {#if event.details}
                    <div class="col-span-2">
                      <span class="text-slate-400">Details:</span>
                      <pre class="mt-1 p-2 bg-slate-100 rounded-sm text-slate-600 overflow-x-auto">{JSON.stringify(event.details, null, 2)}</pre>
                    </div>
                  {/if}
                </div>
              </div>
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  </div>

  <!-- Pagination -->
  {#if getTotalPages() > 1}
    <div class="px-5 py-3 border-t border-slate-100 flex items-center justify-between">
      <button
        type="button"
        onclick={() => goToPage(currentPage - 1)}
        disabled={currentPage === 1}
        aria-label="Previous page"
        class="text-sm text-slate-600 hover:text-slate-900 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
      >
        Previous
      </button>
      <span class="text-sm text-slate-500">
        Page {currentPage} of {getTotalPages()}
      </span>
      <button
        type="button"
        onclick={() => goToPage(currentPage + 1)}
        disabled={currentPage === getTotalPages()}
        aria-label="Next page"
        class="text-sm text-slate-600 hover:text-slate-900 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
      >
        Next
      </button>
    </div>
  {/if}
</div>

<style>
  .event-list {
    transition: box-shadow var(--duration-normal) var(--ease-default);
  }

  .event-list:hover {
    box-shadow: var(--shadow-md);
  }

  .event-item {
    transition: background-color var(--duration-fast) var(--ease-default);
  }
</style>
