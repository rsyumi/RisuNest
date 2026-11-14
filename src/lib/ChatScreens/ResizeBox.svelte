<script>
    import { CharEmotion, ViewBoxsize } from '../../ts/stores.svelte';
    import TransitionImage from './TransitionImage.svelte';
    import { getEmotion } from '../../ts/util';
    
    import { DBState } from 'src/ts/stores.svelte';

    let box = $state();
    // The pointer that is resizing the box; the handle captures it, so moves outside the handle still arrive.
    let resizePointer = null;
    let initialWidth;
    let initialHeight;
    let initialX;
    let initialY;

    function handleStart(event) {
        if (resizePointer !== null && resizePointer !== event.pointerId) return;
        resizePointer = event.pointerId;
        event.currentTarget.setPointerCapture(event.pointerId);
        initialWidth = box.clientWidth;
        initialHeight = box.clientHeight;
        initialX = event.clientX;
        initialY = event.clientY;
    }

    function handleEnd(event) {
        if (event.pointerId === resizePointer) resizePointer = null;
    }

    function handleMove(event) {
        if (event.pointerId !== resizePointer) return;
        event.preventDefault();

        const clientX = event.clientX;
        const clientY = event.clientY;
        const deltaX = initialX - clientX;
        const deltaY = clientY - initialY;

        const newWidth = Math.min(initialWidth + deltaX, window.innerWidth * 0.8);
        const newHeight = Math.min(initialHeight + deltaY, window.innerHeight * 0.8);

        ViewBoxsize.set({
            width: newWidth,
            height: newHeight
        })
    }
</script>

<style>
    .box {
        position: absolute;
        right: 0px;
        top: 0px;
        border-bottom: 1px solid var(--risu-theme-borderc);
        border-left: 1px solid var(--risu-theme-borderc);
        width: 12rem;
        height: 12rem;
        z-index: 5;
    }

    .resize-handle {
        position: absolute;
        width: 16px;
        height: 16px;
        border-top: 1px solid var(--risu-theme-borderc);
        border-right: 1px solid var(--risu-theme-borderc);
        cursor: sw-resize;
        bottom: 0;
        left: 0;
        z-index: 10;
        touch-action: none;
    }

    /* A finger-sized hit area centered on the handle; it draws nothing. */
    .resize-handle::before {
        content: '';
        position: absolute;
        left: -8px;
        bottom: -8px;
        width: 32px;
        height: 32px;
    }
</style>

<div class="box bg-darkbg/70" bind:this="{box}" style="width: {$ViewBoxsize.width}px; height: {$ViewBoxsize.height}px;">
    <!-- Your content here -->
    <TransitionImage classType='risu' src={getEmotion(DBState.db, $CharEmotion, 'plain')}/>
    <div role="button" tabindex="0"
      class="resize-handle"
      onpointerdown={handleStart}
      onpointermove={handleMove}
      onpointerup={handleEnd}
      onpointercancel={handleEnd}
      onlostpointercapture={handleEnd}
    ></div>
</div>