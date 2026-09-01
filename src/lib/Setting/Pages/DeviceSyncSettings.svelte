<script lang="ts">
    import { isTauriAndroid } from 'src/ts/platform'
    import PeerCloneSettings from './PeerCloneSettings.svelte'
    import PeerCloneAndroidSettings from './PeerCloneAndroidSettings.svelte'
</script>

{#if isTauriAndroid}
  <PeerCloneAndroidSettings />
{:else}
  <PeerCloneSettings />
{/if}
