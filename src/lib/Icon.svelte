<script lang="ts">
  let { name, size = 17 }: { name: string; size?: number } = $props();
</script>

<svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  {#if name === 'home'}
    <path d="M4 11.5 12 5l8 6.5V19a1 1 0 0 1-1 1h-4.5v-5h-5v5H5a1 1 0 0 1-1-1Z" />
  {:else if name === 'settings'}
    <circle cx="12" cy="12" r="3" /><path d="M12 3v2.2M12 18.8V21M4.2 7.5l1.9 1.1M17.9 15.4l1.9 1.1M4.2 16.5l1.9-1.1M17.9 8.6l1.9-1.1" /><circle cx="12" cy="12" r="7.2" />
  {:else if name === 'models'}
    <path d="M4 7.5 12 3l8 4.5v9L12 21l-8-4.5Z" /><path d="M4 7.5 12 12l8-4.5M12 12v9" />
  {:else if name === 'folder'}
    <path d="M3.5 7a1.5 1.5 0 0 1 1.5-1.5h4l2 2h8a1.5 1.5 0 0 1 1.5 1.5v8.5A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5Z" />
  {:else if name === 'panel'}
    <rect x="3.5" y="4.5" width="17" height="15" rx="2" /><path d="M9 4.5v15" />
  {:else if name === 'globe'}
    <circle cx="12" cy="12" r="8.5" /><path d="M3.5 12h17M12 3.5c2.4 2.4 3.5 5.2 3.5 8.5s-1.1 6.1-3.5 8.5c-2.4-2.4-3.5-5.2-3.5-8.5s1.1-6.1 3.5-8.5Z" />
  {:else if name === 'chip'}
    <rect x="6" y="6" width="12" height="12" rx="2" /><path d="M9.5 3v3M14.5 3v3M9.5 18v3M14.5 18v3M3 9.5h3M3 14.5h3M18 9.5h3M18 14.5h3" />
  {:else if name === 'chat'}
    <path d="M5 18.5V7a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H8.5Z" />
  {:else if name === 'dots'}
    <circle cx="6" cy="12" r="1" /><circle cx="12" cy="12" r="1" /><circle cx="18" cy="12" r="1" />
  {:else if name === 'chevron'}
    <path d="m7 10 5 5 5-5" />
  {:else if name === 'check'}
    <path d="m5 12.5 4.5 4.5L19 7.5" />
  {:else if name === 'play'}
    <path d="M8.5 5.8v12.4a.6.6 0 0 0 .9.5l9.8-6.2a.6.6 0 0 0 0-1L9.4 5.3a.6.6 0 0 0-.9.5Z" fill="currentColor" stroke="none" />
  {:else if name === 'retry'}
    <path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4.5v4h-4" />
  {:else if name === 'gpu'}
    <rect x="2.5" y="7" width="19" height="10" rx="2" /><circle cx="15.5" cy="12" r="2.5" /><path d="M6 10.5h4M6 13.5h4M5 17v2.5" />
  {:else if name === 'disk'}
    <ellipse cx="12" cy="6.5" rx="7.5" ry="3" /><path d="M4.5 6.5v11c0 1.7 3.4 3 7.5 3s7.5-1.3 7.5-3v-11M4.5 12c0 1.7 3.4 3 7.5 3s7.5-1.3 7.5-3" />
  {:else if name === 'sparkle'}
    <path d="M12 3.5 13.8 10.2 20.5 12 13.8 13.8 12 20.5 10.2 13.8 3.5 12 10.2 10.2Z" />
  {:else if name === 'memory'}
    <path d="M8 4.5h8M6.5 8h11M5 11.5h14v8H5Z" />
  {:else if name === 'eye'}
    <path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z" /><circle cx="12" cy="12" r="3" />
  {:else if name === 'image'}
    <rect x="3.5" y="4.5" width="17" height="15" rx="2" /><circle cx="9" cy="10" r="1.8" /><path d="m20.5 15.5-4.5-4.5-8.5 8.5" />
  {:else if name === 'sliders'}
    <path d="M4 7h10M18 7h2M4 17h4M12 17h8" /><circle cx="16" cy="7" r="2" /><circle cx="10" cy="17" r="2" />
  {:else if name === 'plus'}
    <path d="M12 5v14M5 12h14" />
  {:else if name === 'tag'}
    <path d="M3.5 12.2V4.5a1 1 0 0 1 1-1h7.7l8.3 8.3a1 1 0 0 1 0 1.4l-7.3 7.3a1 1 0 0 1-1.4 0Z" /><circle cx="8" cy="8" r="1.4" />
  {:else if name === 'cap'}
    <path d="M2.5 9.5 12 5l9.5 4.5L12 14Z" /><path d="M6.5 11.5v4.2c1.4 1.3 3.3 2 5.5 2s4.1-.7 5.5-2v-4.2M21.5 9.5v5" />
  {:else if name === 'send'}
    <path d="M4.5 12 20 4.5 15.5 20l-3-6.5Z" /><path d="m12.5 13.5 7.5-9" />
  {:else if name === 'stop'}
    <rect x="7" y="7" width="10" height="10" rx="1.5" fill="currentColor" stroke="none" />
  {:else if name === 'trash'}
    <path d="M5 7h14M10 7V5h4v2M7 7l1 12h8l1-12" />
  {:else if name === 'users'}
    <circle cx="9" cy="8.5" r="3.2" /><path d="M3.5 19c.6-3.2 2.8-5 5.5-5s4.9 1.8 5.5 5" /><path d="M15.5 5.6a3 3 0 0 1 0 5.8M17.5 14.4c1.6.6 2.7 2.2 3 4.6" />
  {:else if name === 'book'}
    <path d="M5 5.5A1.5 1.5 0 0 1 6.5 4H19v14H6.5A1.5 1.5 0 0 0 5 19.5Z" /><path d="M5 19.5A1.5 1.5 0 0 0 6.5 21H19v-3" />
  {:else if name === 'wand'}
    <path d="m4 20 11-11M14 4v2M18 6l-1.4 1.4M20 10h-2M10 6 8.6 4.6" />
  {/if}
</svg>
