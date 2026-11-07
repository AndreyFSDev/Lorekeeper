<script lang="ts">
  // Same ruby heart as on the shortcut icon and in the tray (tools/build-icons.cjs).
  import logoUrl from '../../src-tauri/icons/128x128@2x.png';

  let { size = 32 }: { size?: number } = $props();
</script>

<img class="app-logo" src={logoUrl} width={size} height={size} alt="Cloufam" draggable="false" />

<style>
  .app-logo {
    display: block;
    flex: none;
    user-select: none;
  }
</style>

