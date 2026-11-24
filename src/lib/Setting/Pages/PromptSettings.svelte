<script lang="ts">
    import { ArrowLeft, ChevronsDownUpIcon, ChevronsUpDownIcon, PlusIcon, SearchIcon, TrashIcon } from "@lucide/svelte";
    import { language } from "src/lang";
    import PromptDataItem from "src/lib/UI/PromptDataItem.svelte";
    import { tokenizePreset, type PromptItem, type PromptType } from "src/ts/process/prompt";
    import { newPromptBlock, promptBlockTypeName, promptBlockTypes } from "src/ts/process/promptBlockTypes";
    import { templateCheck } from "src/ts/process/templates/templateCheck";
    
    import { presetEditorDBState as DBState } from 'src/ts/storage/presetEditor.svelte';
    import Check from "src/lib/UI/GUI/CheckInput.svelte";
    import Button from "src/lib/UI/GUI/Button.svelte";
    import TextInput from "src/lib/UI/GUI/TextInput.svelte";
    import NumberInput from "src/lib/UI/GUI/NumberInput.svelte";
    import Help from "src/lib/Others/Help.svelte";
    import TextAreaInput from "src/lib/UI/GUI/TextAreaInput.svelte";
    import PresetToggleEditor from "./PresetToggleEditor.svelte";
    import DependentOptions from "./DependentOptions.svelte";
    import SelectInput from "src/lib/UI/GUI/SelectInput.svelte";
    import OptionInput from "src/lib/UI/GUI/OptionInput.svelte";
    import Accordion from "src/lib/UI/Accordion.svelte";
    import ModelList from "src/lib/UI/ModelList.svelte";
    import { onDestroy, onMount, tick, untrack } from "svelte";
    import {defaultAutoSuggestPrompt} from "../../../ts/storage/defaultPrompts";
    import AuxModelSelectors from './Model/AuxModelSelectors.svelte'
    import { htmlTouchDrag } from "src/ts/gui/htmlTouchDrag";
    import { backNavigationLayer, receivesInput } from "src/ts/ui/modalNavigation";
    import PromptSearchBar from "src/lib/UI/PromptSearchBar.svelte";
    import PromptSearchMatches, { type PromptSearchLine } from "src/lib/UI/PromptSearchMatches.svelte";
    import { blockMatches, buildMatcher, fieldText, matchLine, replaceAllInBlocks, replaceOneInBlock, undoWrites, type PromptFieldChange, type PromptFieldMatch, type PromptFieldWrite, type PromptSearchField, type TextRange } from "src/ts/process/promptSearch";
    import { isCompositionKey, shortcutModifier } from "src/ts/hotkeyModifier";

    let warns: string[] = $state([])
    let tokens = $state(0)
    let extokens = $state(0)
    let draggedIndex = $state(-1)
    let dragOverIndex = $state(-1)
    let openedItems = $state(new Set<PromptItem>())
    // One count runs at a time; a change while it runs queues one more count, and an outdated count is not shown.
    let tokenizing = false
    let tokenizeQueued: PromptItem[] | null = null
    executeTokenize(DBState.db.promptTemplate)
  interface Props {
    onGoBack?: () => void;
    mode?: 'independent'|'inline';
    subMenu?: number;
  }

  let { onGoBack = () => {}, mode = 'independent', subMenu = $bindable(0) }: Props = $props();

    async function executeTokenize(prest: PromptItem[]){
        tokenizeQueued = prest
        if (tokenizing) return
        tokenizing = true
        try {
            while (tokenizeQueued) {
                const template = tokenizeQueued
                tokenizeQueued = null
                const fixed = await tokenizePreset(template, true, DBState.db)
                const exact = await tokenizePreset(template, false, DBState.db)
                if (!tokenizeQueued) {
                    tokens = fixed
                    extokens = exact
                }
            }
        } finally {
            tokenizing = false
        }
    }

    $effect.pre(() => {
    warns = templateCheck(DBState.db)
  });
  $effect.pre(() => {
    // The count reads blocks across awaits, so every counted field is read here for the count to follow it.
    for (const prompt of DBState.db.promptTemplate) void [prompt.type, (prompt as { text?: string }).text, (prompt as { innerFormat?: string }).innerFormat]
    executeTokenize(DBState.db.promptTemplate)
  });

  function getDisplayTemplate() {
    return DBState.db.promptTemplate.map((item, i) => ({
      item,
      originalIndex: i,
      displayIndex: i
    })).filter(entry => !searchShown || searchShown.has(entry.item))
  }

  function handlePromptDrop() {
    if (draggedIndex === -1 || dragOverIndex === -1 || draggedIndex === dragOverIndex) {
      return
    }

    const templates = [...DBState.db.promptTemplate]
    const [movedItem] = templates.splice(draggedIndex, 1)

    const adjustedDropIndex = draggedIndex < dragOverIndex ? dragOverIndex - 1 : dragOverIndex
    templates.splice(adjustedDropIndex, 0, movedItem)

    DBState.db.promptTemplate = templates
    draggedIndex = -1
    dragOverIndex = -1
  }

  $effect(() => {
    const retained = [...openedItems].filter(item => DBState.db.promptTemplate.includes(item))
    if (retained.length !== openedItems.size) openedItems = new Set(retained)
  })

  let allOpened = $derived(
    DBState.db.promptTemplate.length > 0 && DBState.db.promptTemplate.every(item => openedItems.has(item))
  )

  function toggleAllOpened() {
    openedItems = allOpened ? new Set<PromptItem>() : new Set(DBState.db.promptTemplate)
  }

  let addMenuOpen = $state(false)
  let addMenuUp = $state(true)
  let addMenuRoot: HTMLDivElement | undefined = $state()

  function toggleAddMenu(event: MouseEvent & { currentTarget: HTMLButtonElement }) {
    if (!addMenuOpen) {
      // Open toward the side with more room so the whole menu can be scrolled into view.
      const rect = event.currentTarget.getBoundingClientRect()
      addMenuUp = rect.top > window.innerHeight - rect.bottom
    }
    addMenuOpen = !addMenuOpen
  }

  function addPrompt(type: PromptType) {
    addMenuOpen = false
    let value = DBState.db.promptTemplate ?? []
    value.push(newPromptBlock(type))
    DBState.db.promptTemplate = value
  }

  $effect(() => {
    if (!addMenuOpen) return
    const dismiss = (event: PointerEvent) => {
      if (!(event.target instanceof Node) || !addMenuRoot?.contains(event.target)) addMenuOpen = false
    }
    document.addEventListener('pointerdown', dismiss)
    return () => {
      document.removeEventListener('pointerdown', dismiss)
    }
  })

  // `listing` is set by replacing every match, until the query or an option changes.
  type SearchRecord = { key: string, listing: boolean, changes: PromptFieldChange[], count: number }
  let searchOpen = $state(false)
  let searchQuery = $state('')
  let appliedQuery = $state('')
  let searchReplacement = $state('')
  let searchMatchCase = $state(false)
  let searchRegex = $state(false)
  let searchShowReplace = $state(false)
  // The last replace, for undo and for listing what replacing every match wrote.
  let searchRecord: SearchRecord | null = $state.raw(null)
  // Blocks listed while open, and blocks a single replace changed, stay listed for the query they were listed under.
  let keptOpen = $state.raw({ key: '', items: new Set<PromptItem>() })
  let keptReplaced = $state.raw({ key: '', items: new Set<PromptItem>() })
  let searchBar: { focusQuery?: () => void } | undefined = $state()
  let promptListDom: HTMLDivElement | undefined = $state()
  let promptListArea: HTMLDivElement | undefined = $state()

  $effect(() => {
    const query = searchQuery
    if (query === untrack(() => appliedQuery)) return
    const timer = setTimeout(() => { appliedQuery = query }, 200)
    return () => clearTimeout(timer)
  })

  const searchMatcher = $derived(buildMatcher(searchOpen ? appliedQuery : '', { matchCase: searchMatchCase, regex: searchRegex }))
  const searchKey = $derived(`${searchMatchCase}:${searchRegex}:${appliedQuery}`)
  const searchResults = $derived.by(() => {
    if (searchMatcher.kind !== 'ready') return null
    const pattern = searchMatcher.pattern
    return new Map(DBState.db.promptTemplate.map(item => [item, blockMatches(item, pattern)] as const))
  })
  const searchCounts = $derived.by(() => {
    if (!searchResults) return null
    let matches = 0
    let blocks = 0
    for (const found of searchResults.values()) {
      matches += found.length
      if (found.length > 0) blocks++
    }
    return { matches, blocks }
  })
  const changesView = $derived(searchResults && searchRecord?.listing && searchRecord.key === searchKey ? searchRecord : null)
  $effect(() => {
    const key = searchKey
    untrack(() => {
      if (searchRecord?.listing && searchRecord.key !== key) searchRecord = { ...searchRecord, listing: false }
    })
  })
  const searchShown = $derived.by(() => {
    if (!searchResults) return null
    const template = DBState.db.promptTemplate
    if (changesView) {
      const changed = new Set(changesView.changes.map(change => change.block))
      return new Set(template.filter(item => changed.has(item)))
    }
    const open = keptOpen.key === searchKey ? keptOpen.items : null
    const replaced = keptReplaced.key === searchKey ? keptReplaced.items : null
    return new Set(template.filter(item => (searchResults.get(item)?.length ?? 0) > 0
      || (open?.has(item) && openedItems.has(item)) || replaced?.has(item)))
  })

  $effect(() => {
    if (!searchResults) return
    const key = searchKey
    const opened = openedItems
    const listedOpen = [...searchResults].filter(([item, found]) => found.length > 0 && opened.has(item)).map(([item]) => item)
    untrack(() => {
      const kept = keptOpen.key === key ? [...keptOpen.items].filter(item => opened.has(item)) : []
      const next = new Set([...kept, ...listedOpen])
      if (keptOpen.key !== key || next.size !== keptOpen.items.size || [...next].some(item => !keptOpen.items.has(item))) {
        keptOpen = { key, items: next }
      }
    })
  })

  // The search closes on leaving the list, and with the last block, which takes the toolbar the bar sits under.
  $effect(() => {
    if (searchOpen && (subMenu !== 0 || DBState.db.promptTemplate.length === 0)) untrack(closeSearch)
  })

  const editedPreset = $derived(DBState.db.botPresets?.[DBState.db.botPresetsId]?.id ?? DBState.db.botPresetsId)
  $effect(() => {
    void editedPreset
    untrack(() => { searchRecord = null })
  })
  // The template can be swapped without the preset changing, so the undo also ends once none of its blocks is left.
  $effect(() => {
    if (searchRecord && !searchRecord.changes.some(change => DBState.db.promptTemplate.includes(change.block))) {
      untrack(() => { searchRecord = null })
    }
  })

  function closeSearch() {
    searchOpen = false
    searchQuery = ''
    appliedQuery = ''
    searchRecord = null
    keptOpen = { key: '', items: new Set() }
    keptReplaced = { key: '', items: new Set() }
  }

  async function openSearch() {
    searchOpen = true
    addMenuOpen = false
    await tick()
    searchBar?.focusQuery?.()
  }

  async function focusFirstSearchLine() {
    appliedQuery = searchQuery
    await tick()
    promptListDom?.querySelector<HTMLElement>('[data-prompt-search-line]')?.focus()
  }

  function writeSearchFields(writes: PromptFieldWrite[]) {
    const template = DBState.db.promptTemplate
    for (const write of writes) {
      const index = template.indexOf(write.block)
      if (index !== -1) (template[index] as unknown as Record<string, string>)[write.field] = write.value
    }
  }

  function recordSearchReplace(changes: PromptFieldChange[], all: boolean) {
    writeSearchFields(changes.map(change => ({ block: change.block, field: change.field, value: change.after })))
    searchRecord = { key: searchKey, listing: all, changes, count: changes.reduce((sum, change) => sum + change.written.length, 0) }
  }

  function replaceAllSearchMatches() {
    appliedQuery = searchQuery
    const changes = replaceAllInBlocks(DBState.db.promptTemplate, searchMatcher, searchReplacement)
    if (changes.length > 0) recordSearchReplace(changes, true)
  }

  function replaceSearchMatch(item: PromptItem, match: PromptFieldMatch) {
    const change = replaceOneInBlock(item, match.field, match, searchMatcher, searchReplacement)
    if (!change) return
    keptReplaced = { key: searchKey, items: new Set([...(keptReplaced.key === searchKey ? keptReplaced.items : []), item]) }
    recordSearchReplace([change], false)
  }

  function undoSearchReplace() {
    if (searchRecord) writeSearchFields(undoWrites(searchRecord.changes, DBState.db.promptTemplate))
    searchRecord = null
  }

  const searchFieldLabel = (field: PromptSearchField) =>
    field === 'name' ? language.name : field === 'text' ? language.prompt : field === 'innerFormat' ? language.innerFormat : language.defaultPrompt

  function searchCountFor(item: PromptItem) {
    if (!searchShown) return undefined
    if (changesView) return changesView.changes.filter(change => change.block === item).reduce((sum, change) => sum + change.written.length, 0)
    return searchResults?.get(item)?.length ?? 0
  }

  function searchLinesFor(item: PromptItem, reveal: (field: PromptSearchField, range: TextRange | null) => void): PromptSearchLine[] {
    if (changesView) {
      return changesView.changes.filter(change => change.block === item).flatMap(change => change.written.map(range => ({
        label: searchFieldLabel(change.field),
        ...matchLine(change.after, range),
        // A field edited since the replace is focused without a selection.
        onPick: () => reveal(change.field, fieldText(item, change.field) === change.after ? range : null),
      })))
    }
    return (searchResults?.get(item) ?? []).map(match => ({
      label: searchFieldLabel(match.field),
      ...matchLine(fieldText(item, match.field) ?? '', match),
      onPick: () => reveal(match.field, match),
      onReplace: searchShowReplace ? () => replaceSearchMatch(item, match) : undefined,
    }))
  }

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.ctrlKey && e.altKey && e.key === 'o') {
      toggleAllOpened()
    }
    // Ctrl+F belongs to the list only while no other screen or dialog is in front of it.
    if (subMenu === 0 && DBState.db.promptTemplate.length > 0 && !e.defaultPrevented && !isCompositionKey(e) && shortcutModifier(e) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === 'f'
      && promptListArea && receivesInput(promptListArea)) {
      e.preventDefault()
      openSearch()
    }
  }

  onMount(() => {
    document.addEventListener('keydown', handleKeyDown)
  })

  onDestroy(() => {
    document.removeEventListener('keydown', handleKeyDown)
  })
</script>
{#if mode === 'independent'}
    <h2 class="mb-2 text-2xl font-bold mt-2 items-center flex">
        <button class="mr-2 text-textcolor2 hover:text-textcolor" onclick={onGoBack}>
            <ArrowLeft />
        </button>
        {language.promptTemplate}
    </h2>

    <div class="flex w-full rounded-md border border-selected">
        <button onclick={() => {
            subMenu = 0
        }} class="p-2 flex-1" class:bg-selected={subMenu === 0}>
            <span>{language.template}</span>
        </button>
        <button onclick={() => {
            subMenu = 1
        }} class="p-2 flex-1" class:bg-selected={subMenu === 1}>
            <span>{language.settings}</span>
        </button>
    </div>
{/if}
{#if warns.length > 0 && subMenu === 0}
    <div class="text-red-500 flex flex-col items-start p-2 rounded-md border-red-500 border mt-4">
        <h2 class="text-xl font-bold">Warning</h2>
        <div class="border-b border-b-red-500 mt-1 mb-2 w-full"></div>
        {#each warns as warn}
            <span class="ml-4">{warn}</span>
        {/each}
    </div>
{/if}

{#if subMenu === 0}
    <!-- The toolbar, the search bar and the list stack among themselves, so none of them paints over a screen outside them. -->
    <div class="isolate flex flex-col" bind:this={promptListArea}>
    {#if DBState.db.promptTemplate.length > 0}
        <div class="relative z-20 flex items-center gap-2 px-3 mt-4">
            <span class="grow text-sm text-textcolor2">{language.risuNest.promptList.count.replace('{0}', String(DBState.db.promptTemplate.length))}</span>
            <button class="hover:bg-darkbg focus:ring-selected border-darkborderc border rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 inline-flex items-center gap-1.5 px-2 py-1 text-sm {searchOpen ? 'bg-selected text-textcolor' : 'bg-transparent text-textcolor2'}" aria-pressed={searchOpen} onclick={() => searchOpen ? closeSearch() : openSearch()}>
                <SearchIcon size={14} />{language.search}
            </button>
            <Button styled="outlined" size="sm" className="inline-flex items-center gap-1.5" onclick={toggleAllOpened}>
                {#if allOpened}
                    <ChevronsDownUpIcon size={14} />{language.collapseAll}
                {:else}
                    <ChevronsUpDownIcon size={14} />{language.expandAll}
                {/if}
            </Button>
        </div>
    {/if}
    {#if searchOpen}
        <PromptSearchBar bind:this={searchBar} bind:query={searchQuery} bind:replacement={searchReplacement}
            bind:matchCase={searchMatchCase} bind:regex={searchRegex} bind:showReplace={searchShowReplace}
            invalid={searchMatcher.kind === 'invalid'} counts={searchCounts} canReplace={!!searchCounts && searchCounts.matches > 0}
            status={searchRecord ? language.risuNest.promptSearch.replaced.replace('{0}', String(searchRecord.count)) : ''}
            onReplaceAll={replaceAllSearchMatches} onUndo={undoSearchReplace} onClose={closeSearch} onSubmit={focusFirstSearchLine} />
    {/if}
    <div class="contain isolate w-full max-w-full mt-2 flex flex-col px-3 rounded-md" use:htmlTouchDrag bind:this={promptListDom}>
        {#if DBState.db.promptTemplate.length === 0}
                <div class="text-textcolor2">No Format</div>
        {/if}
        {#if searchShown?.size === 0}
            <div class="text-textcolor2 text-sm py-2">{language.risuNest.promptSearch.noMatch}</div>
        {/if}
            {#each getDisplayTemplate() as { item: prompt, originalIndex, displayIndex } (prompt)}
                <PromptDataItem
                    bind:promptItem={DBState.db.promptTemplate[originalIndex]}
                    isDragging={draggedIndex === originalIndex}
                    isOpened={openedItems.has(prompt)}
                    bind:draggedIndex
                    bind:dragOverIndex
                    bind:openedItems
                    currentIndex={originalIndex}
                    displayIndex={displayIndex}
                    searchCount={searchCountFor(prompt)}
                    onDrop={handlePromptDrop}
                    onRemove={() => {
                        // The confirmation is asynchronous, so find the block again instead of trusting its old index.
                        let templates = DBState.db.promptTemplate
                        const index = templates.indexOf(prompt)
                        if(index === -1){
                            return
                        }
                        templates.splice(index, 1)
                        DBState.db.promptTemplate = templates

                        draggedIndex = -1
                        dragOverIndex = -1
                    }}
                    moveDown={() => {
                        if(originalIndex === DBState.db.promptTemplate.length - 1){
                            return
                        }
                        let templates = DBState.db.promptTemplate
                        let temp = templates[originalIndex]
                        templates[originalIndex] = templates[originalIndex + 1]
                        templates[originalIndex + 1] = temp
                        DBState.db.promptTemplate = templates
                    }}
                    moveUp={() => {
                        if(originalIndex === 0){
                            return
                        }
                        let templates = DBState.db.promptTemplate
                        let temp = templates[originalIndex]
                        templates[originalIndex] = templates[originalIndex - 1]
                        templates[originalIndex - 1] = temp
                        DBState.db.promptTemplate = templates
                    }}>
                    {#snippet matchLines(reveal)}
                        {#if searchShown}
                            <PromptSearchMatches lines={searchLinesFor(prompt, reveal)} resetKey={changesView ? `${searchKey}:written` : searchKey} />
                        {/if}
                    {/snippet}
                </PromptDataItem>
            {/each}
    </div>
    </div>

    {#if !searchShown}
    <div class="relative mt-2 mx-3 self-start" bind:this={addMenuRoot}>
        {#if addMenuOpen}
            <div class="absolute left-0 z-20 w-60 rounded-md border border-darkborderc bg-darkbg shadow-lg p-1 flex flex-col max-h-72 overflow-y-auto {addMenuUp ? 'bottom-full mb-1' : 'top-full mt-1'}" role="menu" aria-label={language.add} use:backNavigationLayer={{ close: () => { addMenuOpen = false } }}>
                {#each promptBlockTypes(!!DBState.db.promptSettings?.customChainOfThought) as type (type)}
                    <button role="menuitem" class="px-3 py-2 text-left text-sm rounded-sm hover:bg-selected" onclick={() => addPrompt(type)}>{promptBlockTypeName(type)}</button>
                {/each}
            </div>
        {/if}
        <button class="bg-darkbutton hover:bg-selected focus:ring-selected border-darkborderc border text-textcolor rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 px-4 py-2 text-sm inline-flex items-center gap-1.5" aria-haspopup="menu" aria-expanded={addMenuOpen} onclick={toggleAddMenu}><PlusIcon size={16} />{language.add}</button>
    </div>
    {/if}

    <span class="text-textcolor2 text-sm mt-2">{tokens} {language.fixedTokens}</span>
    <span class="text-textcolor2 mb-6 text-sm mt-2">{extokens} {language.exactTokens}</span>
{:else}
    <h3 class="{mode === 'independent' ? 'mt-4' : 'mt-1'} pb-1 font-semibold text-textcolor border-b border-darkborderc">{language.risuNest.promptOptions.messages}</h3>
    <Check bind:check={DBState.db.promptSettings.sendChatAsSystem} name={language.sendChatAsSystem} className="mt-4"/>
    <Check bind:check={DBState.db.promptSettings.sendName} name={language.formatMessageWithName} className="mt-4"/>
    <DependentOptions parentOn={DBState.db.promptSettings.sendName}>
        <span class="text-textcolor mt-4">{language.namedMessageRole} <Help key="namedMessageRole"/></span>
        <SelectInput bind:value={DBState.db.namedMessageRole}>
            <OptionInput value="user">User</OptionInput>
            <OptionInput value="system">System</OptionInput>
            <OptionInput value="assistant">assistant</OptionInput>
        </SelectInput>
        <span class="text-textcolor mt-4">{language.messageNameFormat} <Help key='messageNameFormat' /></span>
        <TextAreaInput placeholder={`<{{char}}\'s Message>\n{{slot}}\n</{{char}}\'s Message>`} bind:value={DBState.db.messageNameTemplate}/>
    </DependentOptions>
    <Check bind:check={DBState.db.promptSettings.trimStartNewChat} name={language.trimStartNewChat} className="mt-4"/>
    <Check bind:check={DBState.db.promptSettings.utilOverride} name={language.utilOverride} className="mt-4"/>
    <span class="text-textcolor mt-4">{language.maxThoughtTagDepth}</span>
    <NumberInput bind:value={DBState.db.promptSettings.maxThoughtTagDepth}/>
    {#if DBState.db.showUnrecommended}
        <Check bind:check={DBState.db.promptSettings.customChainOfThought} name={language.customChainOfThought} className="mt-4">
            <Help unrecommended key='customChainOfThought' />
        </Check>
    {/if}
    <span class="text-textcolor mt-4">{language.postEndInnerFormat}</span>
    <TextInput bind:value={DBState.db.promptSettings.postEndInnerFormat}/>
    <span class="text-textcolor mt-4">{language.systemContentReplacement} <Help key="systemContentReplacement"/></span>
    <TextAreaInput bind:value={DBState.db.systemContentReplacement}/>
    <span class="text-textcolor mt-4">{language.systemRoleReplacement} <Help key="systemRoleReplacement"/></span>
    <SelectInput bind:value={DBState.db.systemRoleReplacement}>
        <OptionInput value="user">User</OptionInput>
        <OptionInput value="assistant">assistant</OptionInput>
    </SelectInput>
    <span class="text-textcolor mt-4">{language.defaultVariables} <Help key='defaultVariables' /></span>
    <TextAreaInput bind:value={DBState.db.templateDefaultVariables}/>

    <h3 class="mt-6 pb-1 font-semibold text-textcolor border-b border-darkborderc">{language.risuNest.promptOptions.output}</h3>
    <Check bind:check={DBState.db.jsonSchemaEnabled} name={language.enableJsonSchema} className="mt-4"/>
    {#if DBState.db.jsonSchemaEnabled}
        <DependentOptions parentOn={DBState.db.jsonSchemaEnabled}>
            <span class="text-textcolor mt-4">{language.jsonSchema} <Help key='jsonSchema' /></span>
            <TextAreaInput bind:value={DBState.db.jsonSchema}/>
            <span class="text-textcolor mt-4">{language.extractJson} <Help key='extractJson' /></span>
            <TextInput bind:value={DBState.db.extractJson}/>
        </DependentOptions>
    {/if}
    <Check bind:check={DBState.db.strictJsonSchema} name={language.strictJsonSchema} className="mt-4"/>
    <Check bind:check={DBState.db.outputImageModal} name={language.outputImageModal} className="mt-4"/>
    <span class="text-textcolor mt-4">{language.predictedOutput}</span>
    <TextAreaInput bind:value={DBState.db.OAIPrediction}/>

    <h3 class="mt-6 pb-1 font-semibold text-textcolor border-b border-darkborderc">{language.risuNest.promptOptions.assistive}</h3>
    <span class="text-textcolor mt-4">{language.autoSuggest} <Help key='autoSuggest' /></span>
    <TextAreaInput bind:value={DBState.db.autoSuggestPrompt} placeholder={defaultAutoSuggestPrompt}/>

    {#if !DBState.db.auxModelUnderModelSettings}
        <AuxModelSelectors />
    {/if}
    
    {#snippet fallbackModelList(arg:'model'|'memory'|'translate'|'emotion'|'otherAx')}
        {#each DBState.db.fallbackModels[arg] as model, i}
            <span class="text-textcolor mt-4">
                {language.model} {i + 1}
            </span>
            <ModelList bind:value={DBState.db.fallbackModels[arg][i]} blankable />
        {/each}
        <div class="flex gap-2">
            <button class="bg-selected text-textcolor p-2 rounded-md" onclick={() => {
                let value = DBState.db.fallbackModels[arg] ?? []
                value.push('')
                DBState.db.fallbackModels[arg] = value
            }}><PlusIcon /></button>
            <button class="bg-red-500 text-white p-2 rounded-md" onclick={() => {
                let value = DBState.db.fallbackModels[arg] ?? []
                value.pop()
                DBState.db.fallbackModels[arg] = value
            }}><TrashIcon /></button>
        </div>
    {/snippet}

    <Accordion name={language.fallbackModel} styled>
        <Check bind:check={DBState.db.fallbackWhenBlankResponse} name={language.fallbackWhenBlankResponse} className="mt-4"/>
        <Check bind:check={DBState.db.doNotChangeFallbackModels} name={language.doNotChangeFallbackModels} className="mt-4"/>

        <Accordion name={language.model} styled>
            {@render fallbackModelList('model')}
        </Accordion>
        <Accordion name={"Memory"} styled>
            {@render fallbackModelList('memory')}
        </Accordion>
        <Accordion name={"Translations"} styled>
            {@render fallbackModelList('translate')}
        </Accordion>
        <Accordion name={"Emotion"} styled>
            {@render fallbackModelList('emotion')}
        </Accordion>
        <Accordion name={"OtherAx"} styled>
            {@render fallbackModelList('otherAx')}
        </Accordion>
    </Accordion>

    {#if mode === 'independent'}
        <PresetToggleEditor className="mt-4" />
    {/if}
{/if}
