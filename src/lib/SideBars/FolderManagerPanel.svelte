<script lang="ts">
    import ColorPicker from 'svelte-awesome-color-picker'
    import { tick } from 'svelte'
    import { language } from 'src/lang'
    import { alertError } from 'src/ts/alert'
    import { getCharImage } from 'src/ts/characters'
    import { imageAliases } from 'src/ts/media/imageFormats'
    import { saveImage } from 'src/ts/storage/database.svelte'
    import { DBState } from 'src/ts/stores.svelte'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import { selectSingleFile } from 'src/ts/util'
    import { canvasLineMeasure, fitFolderIconText } from 'src/ts/gui/folderIconFit'
    import {
        applyFolderDraft,
        chosenTextContrast,
        folderColorAngles,
        folderFillImage,
        folderPresetBaseHex,
        folderPresetColors,
        folderTextStyle,
        minimumTextContrast,
        normalizeHexColor,
        resolveFolderAppearance,
        type FolderColorMode,
        type FolderIconType,
        type FolderPresetColor,
    } from 'src/ts/gui/folderAppearance'
    import {
        closeFolderManager,
        deleteFolderFromManager,
        findFolder,
        folderCharacterCount,
        folderManager,
        saveFolderManager,
        themeColors,
    } from 'src/ts/gui/folderManager.svelte'
    import FolderIconFace from './FolderIconFace.svelte'
    import SidebarAvatar from './SidebarAvatar.svelte'

    type Picker = 'solid' | 'start' | 'end' | 'text'

    const text = language.risuNest.folderManager
    const swatchClass: Record<FolderPresetColor, string> = {
        default: 'bg-darkbg', red: 'bg-red-700/50', yellow: 'bg-yellow-700/50', green: 'bg-green-700/50',
        blue: 'bg-blue-700/50', indigo: 'bg-indigo-700/50', purple: 'bg-purple-700/50', pink: 'bg-pink-700/50',
    }
    const arrows: Record<number, string> = { 90: '→', 135: '↘', 180: '↓', 45: '↗' }

    let innerWidth = $state(window.innerWidth)
    let innerHeight = $state(window.innerHeight)
    let panel: HTMLDivElement | undefined = $state()
    let body: HTMLDivElement | undefined = $state()
    let panelHeight = $state(0)
    let openedHeight = $state(0)
    let openPicker: Picker | null = $state(null)
    let saving = $state(false)

    const draft = $derived(folderManager.draft!)
    const stored = $derived(folderManager.folderId ? findFolder(folderManager.folderId)?.folder ?? null : null)
    const appearance = $derived(stored ? resolveFolderAppearance(applyFolderDraft(stored, draft)) : null)
    const round = $derived(DBState.db.roundIcons)
    const sheet = $derived(innerWidth < 640)
    const left = $derived(Math.max(8, Math.min((folderManager.anchor?.right ?? 80) + 8, innerWidth - 368)))
    // Placed once by its first height, so an opening colour picker scrolls the body instead of moving the panel.
    const top = $derived(Math.max(8, Math.min((folderManager.anchor?.top ?? 8) - 12, innerHeight - openedHeight - 8)))
    // Styles are read once: a style read after every edit recalculates the styles of the whole sidebar.
    const theme = themeColors()
    let panelFont = $state('')
    const clipped = $derived.by(() => {
        if (!appearance || appearance.iconType !== 'text' || !panelFont) return false
        return fitFolderIconText(appearance.text, round, canvasLineMeasure(panelFont)).clipped
    })
    const lowContrast = $derived.by(() => {
        if (!appearance || draft.textMode !== 'manual') return false
        const contrast = chosenTextContrast(appearance, theme)
        return contrast !== null && contrast < minimumTextContrast
    })

    $effect(() => {
        if (panel) panelFont = getComputedStyle(panel).fontFamily
    })

    $effect(() => {
        if (!openedHeight && panelHeight) openedHeight = panelHeight
    })

    // Another device or window may remove the folder while it is open.
    $effect(() => {
        if (!stored) closeFolderManager()
    })

    function chooseIcon(type: FolderIconType) {
        draft.iconType = type
    }

    // A folder without its own colours starts them from what it shows, and they follow that until they are changed.
    const edited = new Set<Picker>()

    function chooseColorMode(mode: FolderColorMode) {
        if (draft.colorMode === 'preset' && mode !== 'preset' && stored && !resolveFolderAppearance(stored).stops) {
            const base = folderPresetBaseHex(draft.preset, theme)
            if (!edited.has('solid')) draft.solid = base
            if (!edited.has('start')) draft.gradient[0] = base
        }
        draft.colorMode = mode
        openPicker = null
    }

    function chooseTextMode(mode: 'auto' | 'manual') {
        if (mode === 'manual' && draft.textMode === 'auto' && appearance && stored && !resolveFolderAppearance(stored).textColor) {
            const automatic = folderTextStyle({ ...appearance, textColor: null }).color
                ?? normalizeHexColor(getComputedStyle(document.documentElement).getPropertyValue('--risu-theme-textcolor')) ?? '#ffffff'
            if (!edited.has('text')) draft.textColor = automatic
        }
        draft.textMode = mode
        if (mode === 'auto' && openPicker === 'text') openPicker = null
    }

    async function togglePicker(picker: Picker) {
        openPicker = openPicker === picker ? null : picker
        if (!openPicker) return
        await tick()
        body?.querySelector('[data-folder-color-picker]')?.scrollIntoView({ block: 'nearest' })
    }

    function pickerColor(picker: Picker): string {
        return picker === 'solid' ? draft.solid : picker === 'start' ? draft.gradient[0] : picker === 'end' ? draft.gradient[1] : draft.textColor
    }

    function setPickerColor(picker: Picker, hex: string | null) {
        const value = normalizeHexColor(hex)
        if (!value || value === pickerColor(picker)) return
        edited.add(picker)
        if (picker === 'solid') draft.solid = value
        else if (picker === 'start') draft.gradient[0] = value
        else if (picker === 'end') draft.gradient[1] = value
        else draft.textColor = value
    }

    /** Resolves false when no file arrived: none was chosen, or it was not an image or could not be read or stored. */
    let choosingImage: Promise<boolean> | null = null

    function chooseImage() {
        choosingImage ??= pickImage()
            .catch((error) => { alertError(error); return false })
            .finally(() => { choosingImage = null })
    }

    async function pickImage() {
        const opened = folderManager.draft
        const before = opened?.imgFile
        // Null also follows a format error, which selectSingleFile has already shown.
        const file = await selectSingleFile(imageAliases, { image: true })
        if (!file) return false
        const saved = await saveImage(file.data)
        // The panel may have closed, opened again, left the image icon type or removed the image meanwhile.
        if (folderManager.draft === opened && opened?.iconType === 'image' && opened.imgFile === before) opened.imgFile = saved
        return true
    }

    async function save() {
        if (saving) return
        saving = true
        const opened = folderManager.draft
        try {
            // A chosen image may still be reading or saving; the folder is saved with it, or stays open when it failed.
            if (choosingImage && !await choosingImage) return
            await saveFolderManager(opened)
        } finally {
            saving = false
        }
    }
</script>

<svelte:window bind:innerWidth bind:innerHeight />

{#snippet segmented(options: [string, string][], current: string, choose: (value: string) => void, label: string)}
    <div class="flex overflow-hidden rounded-md border border-darkborderc" role="group" aria-label={label}>
        {#each options as [value, name], index (value)}
            <button
                type="button"
                class="flex-1 whitespace-nowrap py-1.5 text-sm text-textcolor transition-colors"
                class:border-l={index > 0}
                class:border-darkborderc={index > 0}
                class:bg-selected={current === value}
                aria-pressed={current === value}
                onclick={() => choose(value)}
            >{name}</button>
        {/each}
    </div>
{/snippet}

{#snippet colorDot(picker: Picker, label: string)}
    <button
        type="button"
        class="color-dot size-7 shrink-0 rounded-full"
        class:open={openPicker === picker}
        style:background={pickerColor(picker)}
        aria-label={label}
        aria-expanded={openPicker === picker}
        onclick={() => togglePicker(picker)}
    ></button>
{/snippet}

{#snippet pickerFor(picker: Picker)}
    {#if openPicker === picker}
        <div class="folder-color-picker w-max max-w-full" data-folder-color-picker>
            <ColorPicker
                label=""
                isDialog={false}
                isAlpha={false}
                hex={pickerColor(picker)}
                onInput={(color) => setPickerColor(picker, color.hex)}
            />
        </div>
    {/if}
{/snippet}

{#if appearance}
    <div class="fixed inset-0 z-[1000]" role="presentation">
        <div class="absolute inset-y-0 right-0 bg-black/45" style:left={sheet ? '0px' : `${folderManager.anchor?.right ?? 0}px`}></div>
        <div
            bind:this={panel}
            bind:offsetHeight={panelHeight}
            class="absolute flex flex-col overflow-hidden border border-darkborderc bg-darkbg text-textcolor shadow-2xl"
            class:sheet
            class:desktop={!sheet}
            class:rounded-lg={!sheet}
            style:left={sheet ? null : `${left}px`}
            style:top={sheet ? null : `${top}px`}
            style:max-height={sheet || !openedHeight ? null : `${innerHeight - top - 8}px`}
            role="dialog"
            aria-modal="true"
            aria-label={text.dialog}
            use:modalNavigation={{ close: closeFolderManager }}
        >
            {#if sheet}
                <div class="mx-auto mt-2 h-1 w-9 shrink-0 rounded-full bg-darkborderc"></div>
            {/if}
            <div class="flex shrink-0 items-center gap-3 border-b border-selected px-4 pt-3.5 pb-3">
                <SidebarAvatar
                    src="slot"
                    size="56"
                    rounded={round}
                    bordered
                    name={draft.name}
                    color={appearance.preset}
                    fill={folderFillImage(appearance)}
                    backgroundimg={appearance.iconType === 'image' && draft.imgFile ? () => getCharImage(draft.imgFile!, 'plain') : ''}
                >
                    <FolderIconFace {appearance} {round} />
                </SidebarAvatar>
                <div class="min-w-0">
                    <div class="truncate text-[15px] font-bold">{draft.name}</div>
                    <div class="text-xs text-subtext">{text.characterCount.replace('{0}', String(stored ? folderCharacterCount(stored) : 0))}</div>
                </div>
            </div>

            <div bind:this={body} class="flex min-h-0 flex-col gap-4 overflow-y-auto px-4 py-3.5">
                <label class="flex flex-col gap-2">
                    <span class="text-[13px] text-subtext">{text.name}</span>
                    <input class="field" bind:value={draft.name} />
                </label>

                <div class="flex flex-col gap-2">
                    <span class="text-[13px] text-subtext">{text.icon}</span>
                    {@render segmented([['text', text.iconText], ['image', text.iconImage], ['glyph', text.iconGlyph]], draft.iconType, (value) => chooseIcon(value as FolderIconType), text.icon)}
                    {#if draft.iconType === 'text'}
                        <textarea class="field resize-none" rows="3" aria-label={text.iconTextLabel} placeholder={draft.name} bind:value={draft.iconText}></textarea>
                        {#if clipped}
                            <p class="hint">{text.clipped}</p>
                        {/if}
                    {:else if draft.iconType === 'image'}
                        <div class="flex items-center gap-2.5">
                            {#if draft.imgFile}
                                {#await getCharImage(draft.imgFile, 'plain') then source}
                                    <img src={source} alt="" class="size-11 rounded-md object-cover" />
                                {/await}
                            {/if}
                            <button type="button" class="button" onclick={chooseImage}>{text.selectImage}</button>
                            {#if draft.imgFile}
                                <button type="button" class="button" onclick={() => { draft.imgFile = null }}>{text.removeImage}</button>
                            {/if}
                        </div>
                    {/if}
                </div>

                <div class="flex flex-col gap-2">
                    <span class="text-[13px] text-subtext">{text.color}</span>
                    {@render segmented([['preset', text.colorPreset], ['solid', text.colorSolid], ['gradient', text.colorGradient]], draft.colorMode, (value) => chooseColorMode(value as FolderColorMode), text.color)}
                    {#if draft.colorMode === 'preset'}
                        <div class="flex flex-wrap gap-1.5">
                            {#each folderPresetColors as preset (preset)}
                                <button
                                    type="button"
                                    class="swatch size-7 rounded-md border border-selected {swatchClass[preset]}"
                                    class:on={draft.preset === preset}
                                    aria-label={text.presetNames[preset]}
                                    aria-pressed={draft.preset === preset}
                                    title={text.presetNames[preset]}
                                    onclick={() => { draft.preset = preset }}
                                ></button>
                            {/each}
                        </div>
                    {:else if draft.colorMode === 'solid'}
                        <div class="flex items-center gap-2.5">{@render colorDot('solid', text.solidColor)}</div>
                        {@render pickerFor('solid')}
                    {:else}
                        <div class="flex items-center gap-2.5">
                            {@render colorDot('start', text.gradientStart)}
                            <span class="text-subtext">→</span>
                            {@render colorDot('end', text.gradientEnd)}
                        </div>
                        {@render pickerFor('start')}
                        {@render pickerFor('end')}
                        <div class="flex gap-1.5">
                            {#each folderColorAngles as angle (angle)}
                                <button
                                    type="button"
                                    class="h-[30px] w-[34px] rounded-md border border-darkborderc text-[15px]"
                                    class:bg-selected={draft.angle === angle}
                                    aria-label={text.directions[angle]}
                                    aria-pressed={draft.angle === angle}
                                    title={text.directions[angle]}
                                    onclick={() => { draft.angle = angle }}
                                >{arrows[angle]}</button>
                            {/each}
                        </div>
                    {/if}
                </div>

                {#if draft.iconType === 'text' || draft.iconType === 'glyph'}
                    {@const glyph = draft.iconType === 'glyph'}
                    {@const colorLabel = glyph ? text.iconColor : text.textColor}
                    <div class="flex flex-col gap-2">
                        <span class="text-[13px] text-subtext">{colorLabel}</span>
                        {@render segmented([['auto', text.textAuto], ['manual', text.textManual]], draft.textMode, (value) => chooseTextMode(value as 'auto' | 'manual'), colorLabel)}
                        {#if draft.textMode === 'manual'}
                            <div class="flex items-center gap-2.5">{@render colorDot('text', colorLabel)}</div>
                            {@render pickerFor('text')}
                        {/if}
                        {#if lowContrast}
                            <p class="hint">{glyph ? text.iconLowContrast : text.lowContrast}</p>
                        {/if}
                    </div>
                {/if}
            </div>

            <div class="footer flex shrink-0 items-center gap-2 border-t border-selected px-4 pt-3">
                <button type="button" class="button danger" disabled={saving} onclick={deleteFolderFromManager}>{text.delete}</button>
                <span class="flex-1"></span>
                <button type="button" class="button" onclick={closeFolderManager}>{language.cancel}</button>
                <button type="button" class="button primary" disabled={saving || !draft.name.trim()} onclick={save}>{text.save}</button>
            </div>
        </div>
    </div>
{/if}

<style>
    .sheet {
        left: 0;
        right: 0;
        bottom: 0;
        max-height: 85dvh;
        border-width: 1px 0 0;
        border-radius: 14px 14px 0 0;
    }
    .desktop {
        width: 360px;
    }
    .footer {
        padding-bottom: max(0.75rem, env(safe-area-inset-bottom));
    }
    .field {
        width: 100%;
        border-radius: 0.375rem;
        border: 1px solid var(--risu-theme-darkborderc);
        background: var(--risu-theme-bgcolor);
        padding: 0.5rem 0.625rem;
        font-size: 14px;
        color: var(--risu-theme-textcolor);
    }
    .field:focus {
        border-color: var(--risu-theme-borderc);
        outline: none;
        box-shadow: 0 0 0 2px var(--risu-theme-borderc);
    }
    /* iOS zooms into fields under 16px. */
    .sheet .field {
        font-size: 16px;
    }
    .field::placeholder {
        color: var(--color-subtext);
    }
    .button {
        white-space: nowrap;
        border-radius: 0.375rem;
        border: 1px solid var(--risu-theme-darkborderc);
        background: var(--risu-theme-darkbutton);
        padding: 0.4375rem 1rem;
        font-size: 14px;
        color: var(--risu-theme-textcolor);
    }
    .button:disabled {
        cursor: not-allowed;
        opacity: 0.5;
    }
    .primary {
        border-color: var(--risu-theme-borderc);
        background: var(--risu-theme-selected);
    }
    .danger {
        border-color: color-mix(in srgb, var(--risu-theme-draculared) 50%, transparent);
        background: transparent;
        color: color-mix(in srgb, var(--risu-theme-draculared) 75%, var(--risu-theme-textcolor));
    }
    .hint {
        font-size: 12px;
        color: color-mix(in srgb, #f59e0b 60%, var(--risu-theme-textcolor));
    }
    .swatch.on {
        outline: 2px solid var(--risu-theme-textcolor);
        outline-offset: 2px;
    }
    .color-dot {
        border: 3px solid #fff;
        box-shadow: 0 0 0 1px var(--risu-theme-darkborderc);
    }
    .color-dot.open {
        box-shadow: 0 0 0 2px var(--risu-theme-borderc);
    }
    .folder-color-picker {
        --cp-bg-color: var(--risu-theme-bgcolor);
        --cp-border-color: var(--risu-theme-darkborderc);
        --cp-text-color: var(--risu-theme-textcolor);
        --cp-input-color: var(--risu-theme-darkbutton);
        --cp-button-hover-color: var(--risu-theme-selected);
        --focus-color: var(--risu-theme-borderc);
    }
</style>
