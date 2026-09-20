<script lang="ts">
  import { onMount } from "svelte";
  import { Minus, Square, Copy, X } from "@lucide/svelte";
  import { getCurrentWindow, type Window } from "@tauri-apps/api/window";
  import logo from "./logo.svg";
  let { platform }: { platform: "windows" | "macos" } = $props();
  let maximized = $state(false);
  let current: Window | null = null;
  onMount(() => {
    let unlisten: (() => void) | undefined;
    let stopped = false;
    void (async () => {
      try {
        current = getCurrentWindow();
        maximized = await current.isMaximized();
        const stop = await current.onResized(async () => {
          maximized = (await current?.isMaximized()) ?? false;
        });
        if (stopped) stop();
        else unlisten = stop;
      } catch {
        current = null;
      }
    })();
    return () => {
      stopped = true;
      unlisten?.();
    };
  });
  async function control(action: "minimize" | "maximize" | "close") {
    if (!current) return;
    try {
      if (action === "minimize") await current.minimize();
      else if (action === "maximize") await current.toggleMaximize();
      else await current.close();
    } catch {
      // The window keeps its native controls when the command is unavailable.
    }
  }
</script>

<header class="titlebar" class:mac={platform === "macos"} data-tauri-drag-region="deep">
  {#if platform === "windows"}
    <img src={logo} alt="" />
    <span class="titlebar-title">RisuNest Sync</span>
    <div class="caption-controls">
      <button aria-label="최소화" onclick={() => control("minimize")}><Minus size={16} /></button>
      <button aria-label={maximized ? "이전 크기로 복원" : "최대화"} onclick={() => control("maximize")}
        >{#if maximized}<Copy size={13} />{:else}<Square size={12} />{/if}</button
      >
      <button class="close" aria-label="닫기" onclick={() => control("close")}><X size={17} /></button>
    </div>
  {:else}
    <span class="titlebar-title">RisuNest Sync</span>
  {/if}
</header>
