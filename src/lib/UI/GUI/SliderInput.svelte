<!-- <input
    class="text-textcolor bg-transparent input-text"
    class:mb-4={marginBottom}
    type="range"
    min={min}
    max={max}
    step={step}
    bind:value
    onchange
> -->

<div class="w-full flex" class:mb-4={marginBottom}>
  {#if disableable}

    <div class="relative h-8 border-darkborderc border rounded-full cursor-pointer rounded-r-none border-r-0 flex justify-center items-center pl-2">
      <CheckInput check={value !== -1000 && value !== undefined} margin={false} onChange={(c) => {
        onchange?.()
        if(c) {
          value = min;
        } else {
          value = -1000;
        }
      }}></CheckInput>
    </div>
  {/if}
  <div 
    role="slider"
    tabindex="0"
    aria-valuemin={min}
    aria-valuemax={max}
    aria-valuenow={sliderValue}
    aria-valuetext={displayText}
    aria-label={ariaLabel}
    class="relative w-full h-8 border-darkborderc border rounded-full cursor-pointer"
    class:rounded-l-none={disableable}
    style:background={
      `linear-gradient(to right, var(--risu-theme-darkbutton) 0%, var(--risu-theme-darkbutton) ${sliderPercent}%, var(--risu-theme-darkbg) ${sliderPercent}%, var(--risu-theme-darkbg) 100%)`
    }
    style:touch-action="pan-y"
    onpointerdown={pressPointer}
    onpointermove={movePointer}
    onpointerup={releasePointer}
    onpointercancel={cancelPointer}
    onlostpointercapture={losePointer}
    onkeydown={handleKeydown}
    bind:this={slider}
  >
    <!-- <div 
      class="absolute top-0 left-0 h-8 rounded-full bg-borderc transition-width duration-200"
      style="width: {(value - min) / (max - min) * 100}%;"
    >
    </div> -->
    <span 
      class="absolute top-0 left-4 h-8 rounded-full items-center justify-center flex text-textcolor text-sm"
    >
      {displayText}
    </span>
  </div>
</div>


<script lang="ts">
  import { language } from "src/lang";
  import CheckInput from "./CheckInput.svelte";

    let slider: HTMLDivElement = $state()
    // The pointer that is changing the value. A touch writes nothing until it slides sideways or taps,
    // so a page scroll that starts on the track leaves the value alone.
    let press: { pointerId: number, touch: boolean, x: number, y: number, valueBefore: number, sliding: boolean, tap: boolean } | null = null
    const touchSlop = 6
  interface Props {
    min?: number;
    max?: number;
    value: number;
    marginBottom?: boolean;
    step?: number;
    fixed?: number;
    multiple?: number;
    disableable?: boolean;
    customText?: string|undefined;
    onchange?: Function;
    ariaLabel?: string;
  }

  let {
    min = undefined,
    max = undefined,
    value = $bindable(),
    marginBottom = false,
    step = 1,
    fixed = 0,
    multiple = 1,
    disableable = false,
    customText = undefined,
    onchange,
    ariaLabel
  }: Props = $props();

  let isDisabledValue = $derived(value === -1000 || value === undefined);
  let sliderValue = $derived(isDisabledValue ? min : value);
  let sliderPercent = $derived((sliderValue - min) / (max - min) * 100);
  let displayText = $derived(customText === undefined ? (isDisabledValue ? language.disabled : (value * multiple).toFixed(fixed)) : customText);

    function changeValue(event: PointerEvent) {
        const rect = slider.getBoundingClientRect();
        const x = event.clientX - rect.left;
        console.log(x, rect.width);
        let newValue = ((x / rect.width) * (max - min)) + min;
        newValue = Math.round(newValue / step) * step;
        value = Math.min(Math.max(newValue, min), max);
    }

    function pressPointer(event: PointerEvent) {
        if (press && press.pointerId !== event.pointerId) return
        slider.setPointerCapture(event.pointerId)
        const touch = event.pointerType === 'touch'
        press = { pointerId: event.pointerId, touch, x: event.clientX, y: event.clientY, valueBefore: value, sliding: !touch, tap: touch }
        if (!touch) changeValue(event)
    }

    function movePointer(event: PointerEvent) {
        if (event.pointerId !== press?.pointerId) return
        if (!press.touch && event.buttons === 0) {
            press = null
            return
        }
        if (!press.sliding) {
            const dx = Math.abs(event.clientX - press.x)
            const dy = Math.abs(event.clientY - press.y)
            if (Math.max(dx, dy) < touchSlop) return
            press.tap = false
            if (dx <= dy) return
            press.sliding = true
        }
        changeValue(event)
    }

    function releasePointer(event: PointerEvent) {
        if (event.pointerId !== press?.pointerId) return
        if (press.tap) changeValue(event)
        press = null
    }

    // The browser cancels a touch it takes over for scrolling; the value goes back to what it was before the press.
    function cancelPointer(event: PointerEvent) {
        if (event.pointerId !== press?.pointerId) return
        if (value !== press.valueBefore) value = press.valueBefore
        press = null
    }

    function losePointer(event: PointerEvent) {
        if (event.pointerId === press?.pointerId) press = null
    }

    function handleKeydown(event: KeyboardEvent) {
        let newValue = isDisabledValue ? min : value;

        switch (event.key) {
            case 'ArrowLeft':
            case 'ArrowDown':
                newValue -= step;
                break;
            case 'ArrowRight':
            case 'ArrowUp':
                newValue += step;
                break;
            case 'Home':
                newValue = min;
                break;
            case 'End':
                newValue = max;
                break;
            default:
                return;
        }

        event.preventDefault();
        value = Math.min(Math.max(newValue, min), max);
    }
</script>
