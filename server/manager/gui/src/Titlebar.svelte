<script lang="ts">
  import { onMount } from "svelte";
  import { Minus, Square, Copy, X } from "@lucide/svelte";
  import { getCurrentWindow, type Window } from "@tauri-apps/api/window";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";
  import logo from "./logo.svg";
  let { platform }: { platform: "windows" | "macos" } = $props();
  let maximized = $state(false);
  // Windows routes the maximize button through the shell, so its hover state arrives as an event.
  let shellHover = $state(false);
  let current: Window | null = null;
  let bar: HTMLElement;
  onMount(() => {
    const stops: (() => void)[] = [];
    let stopped = false;
    const keep = (stop: () => void) => (stopped ? stop() : stops.push(stop));
    void (async () => {
      try {
        current = getCurrentWindow();
        maximized = await current.isMaximized();
        keep(
          await current.onResized(async () => {
            maximized = (await current?.isMaximized()) ?? false;
          }),
        );
        keep(
          await listen<boolean>("caption-maximize-hover", (event) => {
            shellHover = event.payload;
          }),
        );
      } catch {
        current = null;
      }
    })();
    return () => {
      stopped = true;
      for (const stop of stops) stop();
    };
  });
  function contextMenu(event: MouseEvent) {
    if (!(event.target instanceof Node) || !bar.contains(event.target)) return;
    event.preventDefault();
    if (platform !== "windows" || !current) return;
    void invoke("window_system_menu", { x: event.clientX, y: event.clientY }).catch(() => {});
  }
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

<svelte:window oncontextmenu={contextMenu} />
<header
  bind:this={bar}
  class="titlebar"
  class:mac={platform === "macos"}
  data-tauri-drag-region="deep"
>
  {#if platform === "windows"}
    <img src={logo} alt="" />
    <span class="titlebar-title">RisuNest Sync</span>
    <div class="caption-controls">
      <button aria-label="최소화" onclick={() => control("minimize")}><Minus size={16} /></button>
      <button
        class:hover={shellHover}
        aria-label={maximized ? "이전 크기로 복원" : "최대화"}
        onclick={() => control("maximize")}
        >{#if maximized}<Copy size={13} />{:else}<Square size={12} />{/if}</button
      >
      <button class="close" aria-label="닫기" onclick={() => control("close")}><X size={17} /></button>
    </div>
  {:else}
    <span class="titlebar-title">RisuNest Sync</span>
  {/if}
</header>
