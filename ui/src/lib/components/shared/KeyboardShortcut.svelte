<script lang="ts">
  import { getModifierKey } from '#lib/stores/keyboard.svelte.ts';
  
  interface Props {
    keys: string[];
    size?: 'sm' | 'md' | 'lg';
    variant?: 'default' | 'muted' | 'primary';
  }
  
  let { keys, size = 'md', variant = 'default' }: Props = $props();
  
  // Format key names for display
  function formatKey(key: string): string {
    const keyMap: Record<string, string> = {
      'ctrl': getModifierKey(),
      'cmd': '⌘',
      'command': '⌘',
      'meta': '⌘',
      'alt': '⌥',
      'shift': '⇧',
      'enter': '↵',
      'return': '↵',
      'delete': '⌦',
      'backspace': '⌫',
      'escape': 'Esc',
      'esc': 'Esc',
      'arrowup': '↑',
      'arrowdown': '↓',
      'arrowleft': '←',
      'arrowright': '→',
      'up': '↑',
      'down': '↓',
      'left': '←',
      'right': '→',
      'pageup': 'PgUp',
      'pagedown': 'PgDn',
      'home': 'Home',
      'end': 'End',
      'tab': 'Tab',
      'space': 'Space',
      ' ': 'Space'
    };
    
    const lowerKey = key.toLowerCase();
    return keyMap[lowerKey] || key.toUpperCase();
  }
  
  const sizeClasses = {
    sm: 'text-[10px] px-1 py-0.5 gap-0.5 min-h-[18px]',
    md: 'text-xs px-1.5 py-0.5 gap-1 min-h-[22px]',
    lg: 'text-sm px-2 py-1 gap-1.5 min-h-[28px]'
  };
  
  const variantClasses = {
    default: 'bg-white border-gray-200 text-gray-700',
    muted: 'bg-gray-100 border-gray-200 text-gray-500',
    primary: 'bg-blue-50 border-blue-200 text-blue-700'
  };
  
  const keyVariantClasses = {
    default: 'bg-gray-50',
    muted: 'bg-white',
    primary: 'bg-white'
  };
</script>

<span class="inline-flex items-center font-mono font-medium rounded-sm border {sizeClasses[size]} {variantClasses[variant]}">
  {#each keys as key, i}
    {#if i > 0}
      <span class="text-gray-400">+</span>
    {/if}
    <kbd class="inline-flex items-center justify-center min-w-[1.2em] {keyVariantClasses[variant]}">
      {formatKey(key)}
    </kbd>
  {/each}
</span>
