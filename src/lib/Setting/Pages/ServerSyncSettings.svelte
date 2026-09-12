<script lang="ts">
  import { tick } from "svelte";
  import ServerSyncConnection from "../ServerSync/ServerSyncConnection.svelte";
  import { serverSyncScreenRequest } from "src/ts/storage/sync/serverSyncDeepLink";
  $effect(() => {
    if (!$serverSyncScreenRequest) return;
    void tick().then(() =>
      document
        .getElementById("risunest-server-sync")
        ?.scrollIntoView({ block: "start" }),
    );
  });
</script>

<ServerSyncConnection origin={$serverSyncScreenRequest ? "deep-link" : "settings"} initialNavigation={$serverSyncScreenRequest} />