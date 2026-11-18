<script lang="ts">
    import { ArrowLeft, ChevronsDownUpIcon, ChevronsUpDownIcon, PlusIcon, TrashIcon } from "@lucide/svelte";
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
    import ToggleDefinitionEditor from "src/lib/UI/ToggleDefinitionEditor.svelte";
    import SelectInput from "src/lib/UI/GUI/SelectInput.svelte";
    import OptionInput from "src/lib/UI/GUI/OptionInput.svelte";
    import Accordion from "src/lib/UI/Accordion.svelte";
    import ModelList from "src/lib/UI/ModelList.svelte";
    import { onDestroy, onMount } from "svelte";
    import {defaultAutoSuggestPrompt} from "../../../ts/storage/defaultPrompts";
    import AuxModelSelectors from './Model/AuxModelSelectors.svelte'
    import { htmlTouchDrag } from "src/ts/gui/htmlTouchDrag";
    import { backNavigationLayer } from "src/ts/ui/modalNavigation";

    let warns: string[] = $state([])
    let tokens = $state(0)
    let extokens = $state(0)
    let draggedIndex = $state(-1)
    let dragOverIndex = $state(-1)
    let openedItems = $state(new Set<PromptItem>())
    executeTokenize(DBState.db.promptTemplate)
  interface Props {
    onGoBack?: () => void;
    mode?: 'independent'|'inline';
    subMenu?: number;
  }

  let { onGoBack = () => {}, mode = 'independent', subMenu = $bindable(0) }: Props = $props();

    async function executeTokenize(prest: PromptItem[]){
        tokens = await tokenizePreset(prest, true, DBState.db)
        extokens = await tokenizePreset(prest, false, DBState.db)
    }

    $effect.pre(() => {
    warns = templateCheck(DBState.db)
  });
  $effect.pre(() => {
    executeTokenize(DBState.db.promptTemplate)
  });

  function getDisplayTemplate() {
    return DBState.db.promptTemplate.map((item, i) => ({
      item,
      originalIndex: i,
      displayIndex: i
    }))
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

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.ctrlKey && e.altKey && e.key === 'o') {
      toggleAllOpened()
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
    {#if DBState.db.promptTemplate.length > 0}
        <div class="flex items-center gap-2 px-3 mt-4">
            <span class="grow text-sm text-textcolor2">{language.risuNest.promptList.count.replace('{0}', String(DBState.db.promptTemplate.length))}</span>
            <Button styled="outlined" size="sm" className="inline-flex items-center gap-1.5" onclick={toggleAllOpened}>
                {#if allOpened}
                    <ChevronsDownUpIcon size={14} />{language.collapseAll}
                {:else}
                    <ChevronsUpDownIcon size={14} />{language.expandAll}
                {/if}
            </Button>
        </div>
    {/if}
    <div class="contain w-full max-w-full mt-2 flex flex-col px-3 rounded-md" use:htmlTouchDrag>
        {#if DBState.db.promptTemplate.length === 0}
                <div class="text-textcolor2">No Format</div>
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
                    }} />
            {/each}
    </div>

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

    <span class="text-textcolor2 text-sm mt-2">{tokens} {language.fixedTokens}</span>
    <span class="text-textcolor2 mb-6 text-sm mt-2">{extokens} {language.exactTokens}</span>
{:else}
    <span class="text-textcolor mt-4">{language.postEndInnerFormat}</span>
    <TextInput bind:value={DBState.db.promptSettings.postEndInnerFormat}/>

    <Check bind:check={DBState.db.promptSettings.sendChatAsSystem} name={language.sendChatAsSystem} className="mt-4"/>
    <Check bind:check={DBState.db.promptSettings.sendName} name={language.formatMessageWithName} className="mt-4"/>
    <Check bind:check={DBState.db.promptSettings.trimStartNewChat} name={language.trimStartNewChat} className="mt-4"/>
    <Check bind:check={DBState.db.promptSettings.utilOverride} name={language.utilOverride} className="mt-4"/>
    <Check bind:check={DBState.db.jsonSchemaEnabled} name={language.enableJsonSchema} className="mt-4"/>
    <Check bind:check={DBState.db.outputImageModal} name={language.outputImageModal} className="mt-4"/>

    <Check bind:check={DBState.db.strictJsonSchema} name={language.strictJsonSchema} className="mt-4"/>

    {#if DBState.db.showUnrecommended}
        <Check bind:check={DBState.db.promptSettings.customChainOfThought} name={language.customChainOfThought} className="mt-4">
            <Help unrecommended key='customChainOfThought' />
        </Check>
    {/if}
    <span class="text-textcolor mt-4">{language.maxThoughtTagDepth}</span>
    <NumberInput bind:value={DBState.db.promptSettings.maxThoughtTagDepth}/>
    <span class="text-textcolor mt-4">{language.namedMessageRole} <Help key="namedMessageRole"/></span>
    <SelectInput bind:value={DBState.db.namedMessageRole}>
        <OptionInput value="user">User</OptionInput>
        <OptionInput value="system">System</OptionInput>
        <OptionInput value="assistant">assistant</OptionInput>
    </SelectInput>
    <ToggleDefinitionEditor className="mt-4" value={DBState.db.customPromptTemplateToggle} onChange={(text) => {
        DBState.db.customPromptTemplateToggle = text
    }} />
    <span class="text-textcolor mt-4">{language.defaultVariables} <Help key='defaultVariables' /></span>
    <TextAreaInput bind:value={DBState.db.templateDefaultVariables}/>
    <span class="text-textcolor mt-4">{language.predictedOutput}</span>
    <TextAreaInput bind:value={DBState.db.OAIPrediction}/>
    <span class="text-textcolor mt-4">{language.autoSuggest} <Help key='autoSuggest' /></span>
    <TextAreaInput bind:value={DBState.db.autoSuggestPrompt} placeholder={defaultAutoSuggestPrompt}/>
    <span class="text-textcolor mt-4">{language.messageNameFormat} <Help key='messageNameFormat' /></span>
    <TextAreaInput placeholder={`<{{char}}\'s Message>\n{{slot}}\n</{{char}}\'s Message>`} bind:value={DBState.db.messageNameTemplate}/>
    <span class="text-textcolor mt-4">{language.systemContentReplacement} <Help key="systemContentReplacement"/></span>
    <TextAreaInput bind:value={DBState.db.systemContentReplacement}/>
    <span class="text-textcolor mt-4">{language.systemRoleReplacement} <Help key="systemRoleReplacement"/></span>
    <SelectInput bind:value={DBState.db.systemRoleReplacement}>
        <OptionInput value="user">User</OptionInput>
        <OptionInput value="assistant">assistant</OptionInput>
    </SelectInput>
    {#if DBState.db.jsonSchemaEnabled}
        <span class="text-textcolor mt-4">{language.jsonSchema} <Help key='jsonSchema' /></span>
        <TextAreaInput bind:value={DBState.db.jsonSchema}/>
        <span class="text-textcolor mt-4">{language.extractJson} <Help key='extractJson' /></span>
        <TextInput bind:value={DBState.db.extractJson}/>
    {/if}

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

{/if}
