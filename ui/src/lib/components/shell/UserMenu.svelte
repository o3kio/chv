<script lang="ts">
  import { onMount } from 'svelte';
  import { fade, fly } from 'svelte/transition';
  import { User, Settings, LogOut, Sun, Moon } from 'lucide-svelte';
  import { theme } from '#lib/stores/theme.svelte.ts';
  import { goto } from '$app/navigation';
  import { createAPIClient, clearToken } from '#lib/api/client.ts';
  import { toast } from '#lib/stores/toast.svelte.ts';
  
  interface Props {
    userName?: string;
    userEmail?: string;
  }
  
  let { userName = 'Administrator', userEmail = 'admin@chv.local' }: Props = $props();
  
  let isOpen = $state(false);
  let menuElement = $state<HTMLDivElement | null>(null);
  let triggerElement = $state<HTMLButtonElement | null>(null);
  let focusIndex = $state(-1);
  
  const menuItems = [
    { id: 'profile', label: 'Profile', icon: User, href: '/profile' },
    { id: 'settings', label: 'Settings', icon: Settings, href: '/settings' },
  ];
  
  function handleThemeToggle() {
    theme.toggle();
  }
  
  function toggleMenu() {
    isOpen = !isOpen;
    if (isOpen) {
      focusIndex = 0;
    }
  }
  
  function closeMenu() {
    isOpen = false;
    focusIndex = -1;
  }
  
  function handleItemClick(href: string) {
    closeMenu();
    goto(href);
  }
  
  async function handleLogout() {
    closeMenu();
    
    try {
      const client = createAPIClient();
      await client.logout();
    } catch (err) {
      // Ignore logout errors - we'll clear the token anyway
    } finally {
      clearToken();
      toast.success('Logged out successfully');
      goto('/login');
    }
  }
  
  function handleKeyDown(event: KeyboardEvent) {
    if (!isOpen) {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        toggleMenu();
      }
      return;
    }
    
    switch (event.key) {
      case 'Escape':
        event.preventDefault();
        closeMenu();
        triggerElement?.focus();
        break;
      
      case 'ArrowDown':
        event.preventDefault();
        focusIndex = (focusIndex + 1) % (menuItems.length + 2); // +2 for theme toggle and logout
        break;
      
      case 'ArrowUp':
        event.preventDefault();
        focusIndex = (focusIndex - 1 + menuItems.length + 2) % (menuItems.length + 2);
        break;
      
      case 'Home':
        event.preventDefault();
        focusIndex = 0;
        break;
      
      case 'End':
        event.preventDefault();
        focusIndex = menuItems.length + 1;
        break;
      
      case 'Enter':
      case ' ':
        event.preventDefault();
        if (focusIndex < menuItems.length) {
          handleItemClick(menuItems[focusIndex].href);
        } else if (focusIndex === menuItems.length) {
          handleThemeToggle();
        } else {
          handleLogout();
        }
        break;
    }
  }
  
  function handleClickOutside(event: MouseEvent) {
    if (
      menuElement && 
      !menuElement.contains(event.target as Node) && 
      !triggerElement?.contains(event.target as Node)
    ) {
      closeMenu();
    }
  }
  
  onMount(() => {
    document.addEventListener('click', handleClickOutside);
    return () => document.removeEventListener('click', handleClickOutside);
  });
</script>

<div class="relative">
  <!-- Trigger Button -->
  <button
    bind:this={triggerElement}
    type="button"
    class="w-full flex items-center gap-3 px-2 py-2 rounded-sm hover:bg-neutral-800/50 cursor-pointer transition-colors duration-150 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-primary focus-visible:outline-offset-2"
    onclick={toggleMenu}
    onkeydown={handleKeyDown}
    aria-haspopup="true"
    aria-expanded={isOpen}
    aria-controls="user-menu"
  >
    <div class="w-8 h-8 rounded-full bg-linear-to-br from-primary to-(--color-primary-dark) flex items-center justify-center text-white text-xs font-semibold shrink-0">
      {userName.charAt(0).toUpperCase()}
    </div>
    <div class="flex-1 min-w-0 text-left">
      <div class="text-sm font-medium text-white truncate">{userName}</div>
      <div class="text-[10px] text-neutral-500 truncate">{userEmail}</div>
    </div>
  </button>
  
  <!-- Dropdown Menu -->
  {#if isOpen}
    <div
      bind:this={menuElement}
      id="user-menu"
      class="absolute left-0 right-0 bottom-full mb-2 bg-neutral-800 border border-neutral-700 rounded-lg shadow-xl overflow-hidden z-50"
      role="menu"
      aria-orientation="vertical"
      aria-labelledby="user-menu-trigger"
      tabindex="-1"
      transition:fly={{ y: 8, duration: 200 }}
    >
      <!-- Menu Items -->
      {#each menuItems as item, index}
        {@const Icon = item.icon}
        <button
          type="button"
          class="w-full flex items-center gap-3 px-4 py-2.5 text-sm text-neutral-300 hover:bg-primary/15 hover:text-(--color-primary-light) transition-colors duration-150 focus-visible:outline-hidden focus-visible:bg-primary/15 focus-visible:text-(--color-primary-light) {focusIndex === index ? 'bg-primary/15 text-(--color-primary-light)' : ''}"
          role="menuitem"
          onclick={() => handleItemClick(item.href)}
          tabindex="-1"
        >
          <Icon size={16} />
          <span>{item.label}</span>
        </button>
      {/each}
      
      <!-- Theme Toggle -->
      <button
        type="button"
        class="w-full flex items-center gap-3 px-4 py-2.5 text-sm text-neutral-300 hover:bg-primary/15 hover:text-(--color-primary-light) transition-colors duration-150 focus-visible:outline-hidden focus-visible:bg-primary/15 focus-visible:text-(--color-primary-light) {focusIndex === menuItems.length ? 'bg-primary/15 text-(--color-primary-light)' : ''}"
        role="menuitem"
        onclick={handleThemeToggle}
        tabindex="-1"
      >
        {#if theme.value === 'dark'}<Sun size={16} />{:else}<Moon size={16} />{/if}
        <span>{theme.value === 'dark' ? 'Light Mode' : 'Dark Mode'}</span>
      </button>
      
      <!-- Divider -->
      <div class="h-px bg-neutral-700 my-1"></div>
      
      <!-- Logout -->
      <button
        type="button"
        class="w-full flex items-center gap-3 px-4 py-2.5 text-sm text-neutral-300 hover:bg-red-500/15 hover:text-red-400 transition-colors duration-150 focus-visible:outline-hidden focus-visible:bg-red-500/15 focus-visible:text-red-400 {focusIndex === menuItems.length + 1 ? 'bg-red-500/15 text-red-400' : ''}"
        role="menuitem"
        onclick={handleLogout}
        tabindex="-1"
      >
        <LogOut size={16} />
        <span>Logout</span>
      </button>
    </div>
  {/if}
</div>
