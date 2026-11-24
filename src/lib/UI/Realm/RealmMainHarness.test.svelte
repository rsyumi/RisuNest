<script lang="ts">
    import RealmMain from './RealmMain.svelte'
    import { provideChatWindowLayer } from 'src/lib/ChatScreens/ChatWindowLayer.svelte'

    let { windowLayer }: { windowLayer: HTMLElement } = $props()
    // svelte-ignore state_referenced_locally
    provideChatWindowLayer(() => windowLayer)
</script>

<RealmMain />
