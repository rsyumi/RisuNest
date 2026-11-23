<script lang="ts">
    import type { PromptItem, PromptItemChat, PromptRole, PromptType } from "src/ts/process/prompt";
    import OptionInput from "./GUI/OptionInput.svelte";
    import TextAreaInput from "./GUI/TextAreaInput.svelte";
    import SelectInput from "./GUI/SelectInput.svelte";
    import { language } from "src/lang";
    import NumberInput from "./GUI/NumberInput.svelte";
    import CheckInput from "./GUI/CheckInput.svelte";
    import ArrowDown from "@lucide/svelte/icons/arrow-down";
    import ArrowUp from "@lucide/svelte/icons/arrow-up";
    import GripVertical from "@lucide/svelte/icons/grip-vertical";
    import Trash from "@lucide/svelte/icons/trash";
    import TextInput from "./GUI/TextInput.svelte";
    import { presetEditorDBState as DBState } from 'src/ts/storage/presetEditor.svelte';
    import { RISU_PROMPT_DRAG_TYPE } from "src/ts/dragTypes";
    import { alertConfirm } from "src/ts/alert";
    import { newPromptBlock, promptBlockTypeName, promptBlockTypes } from "src/ts/process/promptBlockTypes";
    
    interface Props {
        promptItem: PromptItem;
        onRemove?: () => void;
        moveUp?: () => void;
        moveDown?: () => void;
        onDrop?: () => void;
        isDragging?: boolean;
        isOpened?: boolean;
        draggedIndex?: number;
        dragOverIndex?: number;
        openedItems?: Set<PromptItem>;
        currentIndex?: number;
        displayIndex?: number;
    }

    let {
        promptItem = $bindable(),
        onRemove = () => {},
        moveUp = () => {},
        moveDown = () => {},
        onDrop = () => {},
        isDragging = false,
        isOpened = false,
        draggedIndex = $bindable(-1),
        dragOverIndex = $bindable(-1),
        openedItems = $bindable(new Set<PromptItem>()),
        currentIndex = -1,
        displayIndex = -1
    }: Props = $props();

    const chatPromptChange = () => {
        const currentprompt = promptItem as PromptItemChat
        if(currentprompt.rangeStart === -1000){
            currentprompt.rangeStart = 0
            currentprompt.rangeEnd = 'end'
        }else{
            currentprompt.rangeStart = -1000
            currentprompt.rangeEnd = 'end'
        }
        promptItem = currentprompt
    }

    const hasPromptBlockRole = (promptItem: PromptItem): promptItem is PromptItem & { role2?: PromptRole } => {
        return promptItem.type === 'persona' || promptItem.type === 'description' || promptItem.type === 'authornote' || promptItem.type === 'memory'
    }

    const isPromptRole = (role: unknown): role is PromptRole => {
        return role === 'user' || role === 'bot' || role === 'system' || role === 'assistant'
    }

    function getName(promptItem:PromptItem){
        return promptItem.name || promptBlockTypeName(promptItem.type)
    }

    const keepsText = (type: PromptType) => type === 'plain' || type === 'jailbreak' || type === 'cot'

    function roleName(role: unknown){
        if(role === 'user') return language.user
        if(role === 'bot' || role === 'assistant') return language.character
        if(role === 'system') return language.systemPrompt
        return ''
    }

    // Only blocks sent with a chosen role carry the role chip. A cache point's role picks messages instead.
    function blockRole(promptItem: PromptItem){
        if(keepsText(promptItem.type)) return roleName((promptItem as { role?: unknown }).role)
        if(hasPromptBlockRole(promptItem)) return roleName(promptItem.role2 ?? 'system')
        return ''
    }

    function blockPreview(promptItem: PromptItem){
        let text: unknown = ''
        if(keepsText(promptItem.type) || promptItem.type === 'chatML'){
            text = (promptItem as { text?: unknown }).text
        } else if(hasPromptBlockRole(promptItem)){
            text = (promptItem as { innerFormat?: unknown }).innerFormat
        }
        return typeof text === 'string' ? text.slice(0, 300).replace(/\s+/g, ' ').trim() : ''
    }

    const typeName = $derived(promptBlockTypeName(promptItem.type))
    const title = $derived(promptItem.name || typeName)
    const role = $derived(blockRole(promptItem))
    const detail = $derived.by(() => {
        const preview = blockPreview(promptItem)
        if(!promptItem.name) return preview
        return preview ? `${typeName} · ${preview}` : typeName
    })

    function toggleOpened(){
        const next = new Set(openedItems)
        if (isOpened) {
            next.delete(promptItem)
        } else {
            next.add(promptItem)
        }
        openedItems = next
    }

    async function confirmRemove(){
        if(await alertConfirm(language.removeConfirm + getName(promptItem))){
            onRemove()
        }
    }

    function changeType(next: PromptType){
        const previous = promptItem.type
        ;(promptItem as { type: PromptType }).type = next
        // Text and role carry over between the three text block types.
        if((promptItem.type === 'plain' || promptItem.type === 'jailbreak' || promptItem.type === 'cot') && !keepsText(previous)){
            promptItem.text = ""
            promptItem.role = "system"
        }
        if(promptItem.type === 'cache'){
            promptItem.depth = 1
            promptItem.role = 'all'
        }
        if(promptItem.type === 'chat'){
            promptItem.rangeStart = -1000
            promptItem.rangeEnd = 'end'
        }
        if(hasPromptBlockRole(promptItem) && !isPromptRole(promptItem.role2)){
            promptItem.role2 = 'system'
        }
        // A field the new type has and the block lacks starts as it does in a new block of that type.
        const block = promptItem as unknown as Record<string, unknown>
        for(const [key, value] of Object.entries(newPromptBlock(next))){
            if(block[key] === undefined) block[key] = value
        }
    }

    const isPromptDrag = (e:DragEvent) => {
        return e.dataTransfer?.types.includes(RISU_PROMPT_DRAG_TYPE) ?? false
    }

    const markPromptDrag = (e:DragEvent) => {
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', 'prompt')
        e.dataTransfer.setData(RISU_PROMPT_DRAG_TYPE, 'true')
    }

</script>

<div class="first:mt-0 w-full h-2" role="doc-pagebreak"
    class:bg-borderc={draggedIndex !== -1 && dragOverIndex === currentIndex}
    ondrop={(e) => {
        if(!isPromptDrag(e)){
            return
        }
        e.preventDefault()
        e.stopPropagation()
        dragOverIndex = currentIndex
        onDrop()
    }}
    ondragenter={(e) => {
        if(isPromptDrag(e)) e.preventDefault()
    }}
    ondragover={(e) => {
        if(!isPromptDrag(e)){
            return
        }
        e.preventDefault()
        e.stopPropagation()
        e.dataTransfer.dropEffect = 'move'
        dragOverIndex = currentIndex
    }}>

</div>
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
    class="flex flex-col border border-selected rounded-md bg-darkbg transition-all duration-200"
    class:opacity-50={isDragging}
    class:scale-95={isDragging}
    class:border-b-borderc={draggedIndex !== -1 && dragOverIndex === currentIndex + 1}

    ondragenter={(e) => {
        if(isPromptDrag(e)) e.preventDefault()
    }}
    ondragover={(e) => {
        if(!isPromptDrag(e)){
            return
        }
        e.preventDefault()
        e.stopPropagation()
        e.dataTransfer.dropEffect = 'move'
        if(draggedIndex === -1) {
            return
        }
        if(draggedIndex === currentIndex) {
            dragOverIndex = currentIndex
            return
        }

        const rect = e.currentTarget.getBoundingClientRect()
        const mouseY = e.clientY
        const elementCenter = rect.top + rect.height / 2

        if (mouseY < elementCenter) {
            dragOverIndex = currentIndex
        } else {
            dragOverIndex = currentIndex + 1
        }
    }}
    ondrop={(e) => {
        if(!isPromptDrag(e)){
            return
        }
        e.preventDefault()
        e.stopPropagation()
        onDrop()
    }}
>
    <div
        class="flex items-center gap-1 px-1"
        draggable="true"
        ondragstart={(e) => {
            draggedIndex = currentIndex
            markPromptDrag(e)

            const dragElement = document.createElement('div')
            dragElement.textContent = getName(promptItem)
            dragElement.className = 'absolute -top-96 -left-96 px-4 py-2 bg-darkbg text-textcolor2 rounded-sm text-sm whitespace-nowrap shadow-lg pointer-events-none z-50'
            document.body.appendChild(dragElement)
            e.dataTransfer?.setDragImage(dragElement, 10, 10)

            setTimeout(() => {
                dragElement.remove()
            }, 0)
        }}
        ondragend={(e) => {
            draggedIndex = -1
            dragOverIndex = -1
        }}
    >
        <span class="shrink-0 w-6 self-stretch flex items-center justify-center text-textcolor2 cursor-grab" aria-hidden="true"><GripVertical size={16} /></span>
        <button class="min-w-0 flex-1 py-2 text-left flex flex-col gap-0.5" aria-expanded={isOpened} onclick={toggleOpened}>
            <span class="flex items-center gap-2 min-w-0"><span class="truncate font-medium">{title}</span>{#if role}<span class="shrink-0 rounded-sm bg-selected px-1.5 text-xs leading-5">{role}</span>{/if}</span>
            {#if detail}
                <span class="truncate text-xs text-textcolor2">{detail}</span>
            {/if}
        </button>
        <div class="flex items-center shrink-0">
            <button class="size-10 flex items-center justify-center rounded-md text-textcolor2 hover:text-textcolor hover:bg-selected transition-colors" title={language.movePresetUp} aria-label={language.movePresetUp} onclick={() => moveUp()}><ArrowUp size={18} /></button>
            <button class="size-10 flex items-center justify-center rounded-md text-textcolor2 hover:text-textcolor hover:bg-selected transition-colors" title={language.movePresetDown} aria-label={language.movePresetDown} onclick={() => moveDown()}><ArrowDown size={18} /></button>
            <span class="mx-1 h-5 w-px bg-darkborderc" aria-hidden="true"></span>
            <button class="size-10 flex items-center justify-center rounded-md text-textcolor2 hover:text-draculared hover:bg-draculared/15 transition-colors" title={language.remove} aria-label={language.remove} onclick={confirmRemove}><Trash size={18} /></button>
        </div>
    </div>
    {#if isOpened}
    <div class="flex flex-col px-4 pb-4 border-t border-darkborderc">
        <span class="mt-4">{language.name}</span>
        <TextInput bind:value={promptItem.name} />
        <span class="mt-2">{language.type} </span>
        <SelectInput value={promptItem.type} onchange={(event) => changeType(event.currentTarget.value as PromptType)}>
            {#each promptBlockTypes(!!DBState.db.promptSettings.customChainOfThought) as type (type)}
                <OptionInput value={type}>{promptBlockTypeName(type)}</OptionInput>
            {/each}
        </SelectInput>

        {#if promptItem.type === 'plain' || promptItem.type === 'jailbreak' || promptItem.type === 'cot'}
            <span>{language.specialType}</span>
            <SelectInput bind:value={promptItem.type2}>
                <OptionInput value="normal">{language.noSpecialType}</OptionInput>
                <OptionInput value="main">{language.mainPrompt}</OptionInput>
                <OptionInput value="globalNote">{language.globalNote}</OptionInput>
            </SelectInput>
            <span>{language.prompt}</span>
            <TextAreaInput highlight bind:value={promptItem.text} />
            <span>{language.role}</span>
            <SelectInput bind:value={promptItem.role}>
                <OptionInput value="user">{language.user}</OptionInput>
                <OptionInput value="bot">{language.character}</OptionInput>
                <OptionInput value="system">{language.systemPrompt}</OptionInput>
            </SelectInput>
        {/if}
        {#if promptItem.type === 'chatML'}
            <span>{language.prompt}</span>
            <TextAreaInput highlight bind:value={promptItem.text} />
        {/if}
        {#if promptItem.type === 'cache'}
            <span>{language.depth}</span>
            <NumberInput bind:value={promptItem.depth} />
            <span>{language.role}</span>
            <SelectInput bind:value={promptItem.role}>
                <OptionInput value="all">{language.all}</OptionInput>
                <OptionInput value="user">{language.user}</OptionInput>
                <OptionInput value="assistant">{language.character}</OptionInput>
                <OptionInput value="system">{language.systemPrompt}</OptionInput>
            </SelectInput>
        {/if}
        {#if promptItem.type === 'chat'}
            {#if promptItem.rangeStart !== -1000}
                <span>{language.rangeStart}</span>
                <NumberInput bind:value={promptItem.rangeStart} />
                <span>{language.rangeEnd}</span>
                {#if promptItem.rangeEnd === 'end'}
                    <NumberInput value={0} marginBottom  disabled/>
                    <CheckInput name={language.untilChatEnd} check={true} onChange={() => {
                        if(promptItem.type === 'chat'){
                            promptItem.rangeEnd = 0
                        }
                    }} />
                {:else}
                    <NumberInput bind:value={promptItem.rangeEnd} marginBottom />
                    <CheckInput name={language.untilChatEnd} check={false} onChange={() => {
                        if(promptItem.type === 'chat'){
                            promptItem.rangeEnd = 'end'
                        }
                    }} />
                {/if}
                {#if DBState.db.promptSettings.sendChatAsSystem}
                    <CheckInput name={language.chatAsOriginalOnSystem} bind:check={promptItem.chatAsOriginalOnSystem}/>
                {/if}
            {/if}
            <CheckInput name={language.advanced} check={promptItem.rangeStart !== -1000} onChange={chatPromptChange} className="my-2"/>
        {/if}
        {#if promptItem.type === 'authornote'}
            <span>{language.defaultPrompt}</span>
            <TextInput bind:value={promptItem.defaultText} />
        {/if}
        {#if promptItem.type === 'persona' || promptItem.type === 'description' || promptItem.type === 'authornote' || promptItem.type === 'memory'}
            {#if !promptItem.innerFormat}
                <CheckInput name={language.customInnerFormat} check={false} className="mt-2" onChange={() => {
                    if(promptItem.type === 'persona' || promptItem.type === 'description' || promptItem.type === 'authornote' || promptItem.type === 'memory'){
                        promptItem.innerFormat = "{{slot}}"
                    }
                }} />
            {:else}
                <span>{language.innerFormat}</span>
                <TextAreaInput highlight bind:value={promptItem.innerFormat}/>
                <CheckInput name={language.customInnerFormat} check={true} className="mt-2" onChange={() => {
                    if(promptItem.type === 'persona' || promptItem.type === 'description' || promptItem.type === 'authornote' || promptItem.type === 'memory'){
                        promptItem.innerFormat = null
                    }
                }} />
            {/if}
        {/if}
        {#if hasPromptBlockRole(promptItem)}
            <span>{language.role}</span>
            <SelectInput value={promptItem.role2 ?? 'system'} onchange={(event) => {
                if(hasPromptBlockRole(promptItem)){
                    promptItem.role2 = event.currentTarget.value as PromptRole
                }
            }}>
                <OptionInput value="user">{language.user}</OptionInput>
                <OptionInput value="bot">{language.character}</OptionInput>
                <OptionInput value="system">{language.systemPrompt}</OptionInput>
            </SelectInput>
        {/if}
    </div>
    {/if}
</div>
