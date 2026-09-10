<script lang="ts">
  import Chat from "../../src/lib/ChatScreens/Chat.svelte";
  import { DBState } from "../../src/ts/stores.svelte";
  import type { StreamingThoughtMode } from "../../src/ts/storage/database.svelte";

  let message = $state("Synthetic initial answer");
  let streaming = $state(false);
  let instance = $state(0);
  let character = $derived({
    ...DBState.db.characters[0],
    type: "simple" as const,
  });

  export function configure(mode: StreamingThoughtMode, defer: boolean) {
    DBState.db.streamingThoughtMode = mode;
    DBState.db.streamingDeferDisplayProcessing = defer;
    message = "Synthetic initial answer";
    streaming = false;
    instance++;
  }

  export function publish(source: string, active = true) {
    // This seam begins at accepted display snapshots, after output processing.
    message = source;
    streaming = active;
  }

  export function sourceMatches(source: string) {
    return message === source;
  }
</script>

<main data-streaming-smoke="synthetic-v1">
  <h1>Android synthetic streaming</h1>
  <section id="streaming-body">
    {#key instance}
      <Chat
        {message}
        rawStreamingText={message}
        isOptimizedStreamingMessage={streaming}
        streamingOptimizationMode="balanced"
        name="Synthetic"
        role="char"
        idx={0}
        totalLength={1}
        isLastMemory={false}
        {character}
      />
    {/key}
  </section>
</main>

<style>
  main {
    background: var(--risu-theme-bgcolor);
    color: var(--risu-theme-textcolor);
    min-height: 100vh;
    padding: 16px;
  }
  h1 {
    font-size: 16px;
    margin-bottom: 16px;
  }
</style>
