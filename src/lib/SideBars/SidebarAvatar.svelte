<script lang="ts">
  import { tooltipRight } from "src/ts/gui/tooltip";
  import { observeNearViewport } from "src/ts/ui/observeNearViewport";

  type ImageSource = string | Promise<string> | (() => string | Promise<string>);

  interface Props {
    rounded: boolean;
    src: ImageSource;
    name: string;
    size?: string;
    onClick?: any;
    bordered?: boolean;
    color?: string;
    backgroundimg?: ImageSource;
    children?: import('svelte').Snippet;
    oncontextmenu?: (event: MouseEvent & {
        currentTarget: EventTarget & HTMLDivElement;
    }) => any
    chaId?: string;
  }

  let {
    rounded,
    src,
    name,
    size = "22",
    onClick = () => {},
    bordered = false,
    color = '',
    backgroundimg = '',
    children,
    oncontextmenu,
    chaId
  }: Props = $props();

  let avatarElement: HTMLSpanElement | null = $state(null);
  let sourceVisible = $state(false);
  let resolvedSrc = $derived.by(() => {
    if (typeof src !== 'function') return src;
    if (!sourceVisible) return '';
    return src();
  });
  let resolvedBackgroundImage = $derived.by(() => {
    if (typeof backgroundimg !== 'function') return backgroundimg;
    if (!sourceVisible) return '';
    return backgroundimg();
  });

  $effect(() => {
    const hasLazySource = typeof src === 'function' || typeof backgroundimg === 'function';
    if (!hasLazySource || sourceVisible || !avatarElement) return;

    return observeNearViewport(avatarElement, () => {
      sourceVisible = true;
    });
  });
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<span class="flex shrink-0 items-center justify-center avatar"
      bind:this={avatarElement}
      class:border = {bordered}
      class:border-selected={bordered}
      class:rounded-md={bordered}
      oncontextmenu={oncontextmenu}
      onclick={onClick} use:tooltipRight={name}
      role="button"
      tabindex="0"
      data-char-id={chaId}
>
  {#if src}
    {#if src === "slot"}
      {#await resolvedBackgroundImage}
      <div
        class="bg-skin-border sidebar-avatar rounded-md bg-top flex items-center justify-center {
          color === 'red' ? 'bg-red-700/50' :
          color === 'yellow' ? 'bg-yellow-700/50' :
          color === 'green' ? 'bg-green-700/50' :
          color === 'blue' ? 'bg-blue-700/50' :
          color === 'indigo' ? 'bg-indigo-700/50' :
          color === 'purple' ? 'bg-purple-700/50' :
          color === 'pink' ? 'bg-pink-700/50' :
          'bg-darkbg/50'
        }"
        style:width={size + "px"}
        style:height={size + "px"}
        style:minWidth={size + "px"}
        class:rounded-md={!rounded} class:rounded-full={rounded}
      ></div>
      {:then resolvedBgImg}
      <div
        class="bg-skin-border sidebar-avatar rounded-md bg-top flex items-center justify-center {
          color === 'red' ? 'bg-red-700/50' :
          color === 'yellow' ? 'bg-yellow-700/50' :
          color === 'green' ? 'bg-green-700/50' :
          color === 'blue' ? 'bg-blue-700/50' :
          color === 'indigo' ? 'bg-indigo-700/50' :
          color === 'purple' ? 'bg-purple-700/50' :
          color === 'pink' ? 'bg-pink-700/50' :
          'bg-darkbg/50'
        }"
        style:width={size + "px"}
        style:height={size + "px"}
        style:minWidth={size + "px"}
        style:background-image={resolvedBgImg ? `url('${resolvedBgImg}')` : undefined}
        style:background-size={resolvedBgImg ? "cover" : undefined}
        style:background-position={resolvedBgImg ? "center" : undefined}
        class:rounded-md={!rounded} class:rounded-full={rounded}
      >
      {#if !resolvedBgImg}
        {@render children?.()}
      {/if}
        </div>
      {:catch}
      <div
        class="bg-skin-border sidebar-avatar rounded-md bg-top flex items-center justify-center {
          color === 'red' ? 'bg-red-700/50' :
          color === 'yellow' ? 'bg-yellow-700/50' :
          color === 'green' ? 'bg-green-700/50' :
          color === 'blue' ? 'bg-blue-700/50' :
          color === 'indigo' ? 'bg-indigo-700/50' :
          color === 'purple' ? 'bg-purple-700/50' :
          color === 'pink' ? 'bg-pink-700/50' :
          'bg-darkbg/50'
        }"
        style:width={size + "px"}
        style:height={size + "px"}
        style:minWidth={size + "px"}
        class:rounded-md={!rounded} class:rounded-full={rounded}
      >
        {@render children?.()}
      </div>
    {/await}
    {:else}
      {#if typeof src === 'function' && !sourceVisible}
        <div
          class="bg-skin-border sidebar-avatar rounded-md bg-top"
          style:width={size + "px"}
          style:height={size + "px"}
          style:minWidth={size + "px"}
          class:rounded-md={!rounded} class:rounded-full={rounded}
        ></div>
      {:else}
      {#await resolvedSrc}
        <div
          class="bg-skin-border sidebar-avatar rounded-md bg-top"
          style:width={size + "px"}
          style:height={size + "px"}
          style:minWidth={size + "px"}
          class:rounded-md={!rounded} class:rounded-full={rounded} 
></div>
      {:then img}
        <img
          src={img}
          class="bg-skin-border sidebar-avatar rounded-md object-cover object-top"
          style:width={size + "px"}
          style:height={size + "px"}
          style:minWidth={size + "px"}
          class:rounded-md={!rounded} class:rounded-full={rounded} 
          alt="avatar"
        />
      {:catch}
        <div
          class="bg-skin-border sidebar-avatar rounded-md bg-top"
          style:width={size + "px"}
          style:height={size + "px"}
          style:minWidth={size + "px"}
          class:rounded-md={!rounded} class:rounded-full={rounded}
        ></div>
      {/await}
      {/if}
    {/if}
  {:else}
    <div
      class="bg-skin-border sidebar-avatar rounded-md bg-top"
      style:width={size + "px"}
      style:height={size + "px"}
      style:minWidth={size + "px"}
      class:rounded-md={!rounded} class:rounded-full={rounded} 
></div>
  {/if}
</span>
