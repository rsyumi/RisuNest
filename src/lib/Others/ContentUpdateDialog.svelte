<script lang="ts">
    // Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/lib/Others/CharacterUpdateDialog.svelte), GPL-3.0.
    import { ChevronRightIcon, FileUpIcon, RefreshCwIcon, SearchIcon, XIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import { summarizePlan, type UpdateItem, type UpdateSide } from 'src/ts/contentUpdate/merge'
    import {
        editableText, isEditable, lineDiff, loreFolderNames, updateGroupOf, updateGroupOrder, updateItemName,
        updateSearchText, updateSideValue, updateSubgroupOf, updateValueText, type UpdateGroupId,
    } from 'src/ts/contentUpdate/view'
    import {
        contentUpdateDialog as dialog, backToContentUpdateTargets, chooseContentUpdate, chooseContentUpdateMany,
        loadContentUpdateBaseFile, reviewContentUpdate, selectContentUpdateTarget, setContentUpdateChoice,
        setContentUpdateEdit, type UpdatePick,
    } from 'src/ts/contentUpdate/flow.svelte'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'

    const text = $derived(language.risuNest.contentUpdate)
    const request = $derived(dialog.request)
    const isModule = $derived(request?.kind === 'module')
    const plan = $derived(dialog.plan)
    const counts = $derived(plan ? summarizePlan(plan, dialog.choices) : null)
    const toDecide = $derived(plan ? plan.items.filter((item) => item.choice === null) : [])
    const editCount = $derived(Object.values(dialog.choices).filter((pick) => pick === 'edit').length)
    const greetingChanged = $derived(plan?.items.some((item) => item.field === 'firstMessage' || item.field === 'alternateGreetings') ?? false)

    let otherQuery = $state('')
    let query = $state('')
    let opened = $state<Record<string, boolean>>({})
    // Section open state the user set; unset sections follow their size.
    let sectionOpen = $state<Record<string, boolean>>({})

    $effect(() => {
        // A new comparison starts with the first item to decide open.
        const first = dialog.plan?.items.find((item) => item.choice === null)
        opened = first ? { [first.id]: true } : {}
        sectionOpen = {}
        query = ''
    })

    $effect(() => {
        if (!request) otherQuery = ''
    })

    const otherMatches = $derived.by(() => {
        const words = otherQuery.trim().toLowerCase().split(/\s+/).filter(Boolean)
        if (!request || !words.length) return []
        const out: { id: string, name: string }[] = []
        for (const other of request.others) {
            const name = other.name.toLowerCase()
            if (words.every((word) => name.includes(word))) out.push(other)
            if (out.length >= 20) break
        }
        return out
    })
    const chosenOther = $derived(request?.others.find((other) => other.id === dialog.target))

    // The labels the character and module editors show for the same fields.
    const fieldLabels = $derived<Record<string, string>>({
        ...text.fields,
        name: language.name, desc: language.description, description: language.description,
        firstMessage: language.firstMessage, alternateGreetings: language.altGreet,
        personality: language.personality, scenario: language.scenario, exampleMessage: language.exampleMessage,
        systemPrompt: language.systemPrompt, replaceGlobalNote: language.replaceGlobalNote,
        creatorNotes: language.creatorNotes, tags: language.tags, creator: language.creator,
        characterVersion: language.CharVersion, nickname: language.nickname,
        globalLore: language.loreBook, lorebook: language.loreBook,
        customscript: language.regexScript, regex: language.regexScript,
        triggerscript: language.triggerScript, trigger: language.triggerScript,
        image: language.charIcon, icon: language.icon, emotionImages: language.emotionImage,
        additionalAssets: language.additionalAssets, assets: language.additionalAssets,
        backgroundHTML: language.backgroundHTML, backgroundEmbedding: language.backgroundHTML,
        additionalText: language.additionalText, defaultVariables: language.defaultVariables,
        lorePlus: language.lorePlus, viewScreen: language.viewScreen, lowLevelAccess: language.lowLevelAccess,
        namespace: language.namespace, hideIcon: language.hideChatIcon, hideChatIcon: language.hideChatIcon,
        customModuleToggle: language.customPromptTemplateToggle, license: language.license, source: language.source,
        cjs: language.charjs, sdData: language.imageGeneration,
        utilityBot: language.utilityBot, largePortrait: language.largePortrait, inlayViewScreen: language.inlayViewScreen,
        depth_prompt: language.depthPrompt, onboarding: language.onboarding,
    })

    function fieldLabel(field: string) {
        return fieldLabels[field] ?? field
    }

    // Built once per comparison: the search reads every value.
    const searchIndex = $derived.by(() => {
        const index = new Map<string, string>()
        for (const item of plan?.items ?? []) index.set(item.id, updateSearchText(item, fieldLabel(item.field)))
        return index
    })

    function matching(source: UpdateItem[]) {
        const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean)
        if (!words.length) return source
        return source.filter((item) => {
            const haystack = searchIndex.get(item.id) ?? ''
            return words.every((word) => haystack.includes(word))
        })
    }

    const visible = $derived(matching(dialog.showAll ? plan?.items ?? [] : toDecide))
    // Matches among items applied on their own, offered when the decide view finds none.
    const hiddenMatches = $derived(!dialog.showAll && query.trim() && !visible.length ? matching(plan?.items ?? []).length : 0)

    type Sub = { id: string, label: string, items: UpdateItem[] }
    type Section = { id: UpdateGroupId, items: UpdateItem[], subs: Sub[] }

    const sections = $derived.by(() => {
        const folders = dialog.folders.size ? dialog.folders : loreFolderNames()
        const byGroup = new Map<UpdateGroupId, Section>()
        for (const item of visible) {
            const group = updateGroupOf(item)
            let section = byGroup.get(group)
            if (!section) byGroup.set(group, section = { id: group, items: [], subs: [] })
            section.items.push(item)
            const sub = updateSubgroupOf(item, folders, fieldLabels)
            let bucket = section.subs.find((entry) => entry.id === sub.id)
            if (!bucket) section.subs.push(bucket = { id: sub.id, label: sub.label, items: [] })
            bucket.items.push(item)
        }
        return updateGroupOrder.map((id) => byGroup.get(id)).filter((section): section is Section => !!section)
    })

    function subLabel(section: Section, sub: Sub) {
        if (sub.label) return sub.label
        if (section.id === 'lore') return sub.id === 'settings' ? text.loreSettingsGroup : text.loreRootGroup
        return ''
    }

    function isOpen(id: string, size: number) {
        // Searching opens everything it found; otherwise big sections start folded.
        return sectionOpen[id] ?? (query.trim() !== '' || size <= 20)
    }

    function toggleSection(id: string, size: number) {
        sectionOpen = { ...sectionOpen, [id]: !isOpen(id, size) }
    }

    function toggle(id: string) {
        opened = { ...opened, [id]: !opened[id] }
    }

    function pickOf(item: UpdateItem): UpdatePick | null {
        return dialog.choices[item.id] ?? item.choice
    }

    function pick(item: UpdateItem, value: UpdatePick) {
        setContentUpdateChoice(item.id, value)
        if (value === 'edit') opened = { ...opened, [item.id]: true }
    }

    function unresolvedIn(items: UpdateItem[]) {
        return items.filter((item) => !pickOf(item)).length
    }

    function chooseScope(items: UpdateItem[], side: UpdateSide) {
        chooseContentUpdateMany(items.map((item) => item.id), side)
    }

    const summaryKeys = ['update', 'mine', 'merged', 'added', 'removed', 'mineOnly', 'conflict', 'differs'] as const
    const segment = 'min-h-8 px-2.5 py-1 text-[13px] whitespace-nowrap text-subtext transition-colors hover:bg-selected hover:text-textcolor'
    const segmentOn = 'bg-selected font-medium text-textcolor'
</script>

{#snippet diffView(before: string, after: string, minus: string, plus: string)}
    <div class="overflow-hidden rounded-md border border-darkborderc" role="group" aria-label="{minus} → {plus}">
        <div class="flex gap-4 border-b border-darkborderc px-2.5 py-1 text-xs text-subtext">
            <span class="text-danger-400">− {minus}</span>
            <span class="text-success-400">+ {plus}</span>
        </div>
        {#if !before && !after}
            <p class="px-2.5 py-2 text-sm text-subtext">{text.absent}</p>
        {:else}
            <pre class="max-h-72 overflow-auto py-1.5 text-xs leading-relaxed break-words whitespace-pre-wrap">{#each lineDiff(before, after) as line}{#if line.type === 'skip'}<span class="block px-2.5 text-subtext italic">⋯ {text.skippedLines(Number(line.text))}</span>{:else if line.type === 'add'}<span class="block bg-success-500/10 px-2.5 text-success-400">+ {line.text}</span>{:else if line.type === 'del'}<span class="block bg-danger-500/10 px-2.5 text-danger-400">− {line.text}</span>{:else}<span class="block px-2.5 text-subtext">  {line.text}</span>{/if}{/each}</pre>
        {/if}
    </div>
{/snippet}

{#snippet scopeButtons(items: UpdateItem[], label: string)}
    <span class="inline-flex shrink-0 overflow-hidden rounded-md border border-darkborderc" role="group" aria-label={label}>
        <button type="button" class={segment} onclick={() => chooseScope(items, 'mine')}>{text.scopeMine}</button>
        <button type="button" class="{segment} border-l border-darkborderc" onclick={() => chooseScope(items, 'theirs')}>{text.scopeTheirs}</button>
    </span>
{/snippet}

{#snippet itemView(item: UpdateItem)}
    {@const current = pickOf(item)}
    {@const name = item.order ? `${fieldLabel(item.field)} ${text.listOrder}` : updateItemName(item)}
    {@const editable = isEditable(item)}
    <li class="min-w-0 border-b border-darkborderc/60 py-1 last:border-b-0" data-update-item={item.id}>
        <div class="flex flex-wrap items-center gap-1.5">
            <button type="button" class="flex min-h-9 min-w-0 flex-[1_1_14rem] items-center gap-1.5 rounded-md px-2 py-1 text-left text-sm hover:bg-selected" aria-expanded={!!opened[item.id]} onclick={() => toggle(item.id)}>
                <ChevronRightIcon size={14} class="shrink-0 text-subtext transition-transform {opened[item.id] ? 'rotate-90' : ''}" />
                {#if !current}<span class="size-2 shrink-0 rounded-full bg-danger-400" title={text.pendingMark}></span>{/if}
                {#if item.key || item.order}<span class="truncate">{name || text.unnamed}</span>{:else}<span class="font-medium">{fieldLabel(item.field)}</span>{/if}
                <span class="ml-auto shrink-0 text-xs {item.status === 'conflict' || item.status === 'differs' ? 'text-danger-400' : item.status === 'added' || item.status === 'update' || item.status === 'merged' ? 'text-success-400' : 'text-subtext'}">{text.status[item.status]}</span>
            </button>
            <span class="inline-flex shrink-0 overflow-hidden rounded-md border border-darkborderc" role="group" aria-label={name || fieldLabel(item.field)}>
                <button type="button" aria-pressed={current === 'mine'} class="{segment} {current === 'mine' ? segmentOn : ''}" onclick={() => pick(item, 'mine')}>{text.keepMine}</button>
                <button type="button" aria-pressed={current === 'theirs'} class="{segment} border-l border-darkborderc {current === 'theirs' ? segmentOn : ''}" onclick={() => pick(item, 'theirs')}>{text.takeTheirs}</button>
                {#if item.status === 'merged'}
                    <button type="button" aria-pressed={current === 'both'} class="{segment} border-l border-darkborderc {current === 'both' ? segmentOn : ''}" onclick={() => pick(item, 'both')}>{text.takeBoth}</button>
                {/if}
                {#if editable}
                    <button type="button" aria-pressed={current === 'edit'} class="{segment} border-l border-darkborderc {current === 'edit' ? segmentOn : ''}" onclick={() => pick(item, 'edit')}>{text.editOwn}</button>
                {/if}
            </span>
        </div>
        {#if opened[item.id]}
            <div class="mt-1 mb-2 ml-5 flex flex-col gap-1.5">
                {#if item.baseKnown}
                    <h4 class="text-xs font-semibold text-subtext">{text.authorChange}</h4>
                    {#if updateValueText(item.base) === updateValueText(item.theirs)}
                        <p class="text-sm text-subtext">{text.authorUnchanged}</p>
                    {:else}
                        {@render diffView(updateValueText(item.base), updateValueText(item.theirs), text.oldSource, text.newSource)}
                    {/if}
                {/if}
                <h4 class="text-xs font-semibold text-subtext">{text.mineVsTheirs}</h4>
                {@render diffView(updateValueText(updateSideValue(item, 'mine')), updateValueText(updateSideValue(item, 'theirs')), text.mineSide, text.theirsSide)}
                {#if item.status === 'merged' && item.resolved}
                    <h4 class="text-xs font-semibold text-subtext">{text.takeBoth}</h4>
                    {@render diffView(updateValueText(item.mine), updateValueText(item.resolved.mine), text.mineSide, text.takeBoth)}
                {/if}
                {#if current === 'edit'}
                    <div class="flex flex-col gap-1">
                        <div class="flex flex-wrap items-center justify-between gap-2">
                            <h4 class="text-xs font-semibold text-subtext">{text.editHeading}</h4>
                            <span class="flex gap-3 text-xs">
                                <button type="button" class="underline underline-offset-2 hover:text-textcolor" onclick={() => setContentUpdateEdit(item.id, editableText(item, 'mine'))}>{text.editFromMine}</button>
                                <button type="button" class="underline underline-offset-2 hover:text-textcolor" onclick={() => setContentUpdateEdit(item.id, editableText(item, 'theirs'))}>{text.editFromTheirs}</button>
                            </span>
                        </div>
                        <textarea
                            aria-label={text.editHeading}
                            class="w-full resize-y rounded-md border border-darkborderc bg-bgcolor px-2.5 py-2 text-sm text-textcolor focus:border-borderc focus:outline-hidden"
                            rows={Math.min(16, Math.max(5, (dialog.edits[item.id] ?? '').split('\n').length + 1))}
                            value={dialog.edits[item.id] ?? ''}
                            oninput={(event) => setContentUpdateEdit(item.id, event.currentTarget.value)}
                        ></textarea>
                    </div>
                {/if}
            </div>
        {/if}
    </li>
{/snippet}

{#if request}
    <div role="presentation" class="fixed inset-0 z-work-dialog-content-update flex items-center justify-center bg-black/60 p-4">
        <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="content-update-title"
            aria-describedby="content-update-description"
            use:modalNavigation={{ close: () => chooseContentUpdate('cancel') }}
            class="flex max-h-[90dvh] w-full flex-col rounded-lg border border-darkborderc bg-darkbg text-textcolor {dialog.step === 'review' ? 'max-w-3xl' : 'max-w-lg'}"
        >
            <header class="flex flex-col gap-1 px-5 pt-5 pb-3">
                <h2 id="content-update-title" class="flex items-center gap-2 text-lg font-bold">
                    <RefreshCwIcon size={20} class="shrink-0" />
                    {dialog.step === 'review' ? text.reviewTitle(dialog.targetName) : isModule ? text.moduleTitle : text.title}
                </h2>
                <p id="content-update-description" class="text-sm text-subtext">
                    {dialog.step === 'review' ? text.reviewDescription : isModule ? text.moduleDescription(request.incomingName) : text.description(request.incomingName)}
                </p>
            </header>

            <div class="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-5 pb-3">
                {#if dialog.step === 'choose'}
                    {#if request.candidates.length}
                        <h3 class="text-xs font-semibold text-subtext">{isModule ? text.moduleCandidates : text.candidates}</h3>
                        <div class="flex flex-col gap-1.5" role="group" aria-label={isModule ? text.moduleCandidates : text.candidates}>
                            {#each request.candidates as candidate (candidate.id)}
                                <button
                                    type="button"
                                    aria-pressed={dialog.target === candidate.id}
                                    class="flex w-full items-center gap-3 rounded-md border px-3 py-2.5 text-left transition-colors {dialog.target === candidate.id ? 'border-borderc bg-selected' : 'border-darkborderc hover:bg-selected/60'}"
                                    disabled={dialog.busy}
                                    onclick={() => selectContentUpdateTarget(candidate.id)}
                                >
                                    <span class="size-4 shrink-0 rounded-full border-2 {dialog.target === candidate.id ? 'border-textcolor bg-textcolor' : 'border-subtext'}" aria-hidden="true"></span>
                                    <span class="flex min-w-0 flex-col gap-0.5">
                                        <strong class="break-words">{candidate.name || text.unnamed}</strong>
                                        <span class="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-subtext">
                                            <span>{text.reasons[candidate.reason]}</span>
                                            {#if candidate.creator}<span>{text.creator(candidate.creator)}</span>{/if}
                                            <span>{isModule ? text.loreEntries(candidate.count) : text.chats(candidate.count)}</span>
                                        </span>
                                    </span>
                                </button>
                            {/each}
                        </div>
                    {/if}
                    {#if request.others.length}
                        <div class="flex flex-col gap-1.5">
                            <h3 class="text-xs font-semibold text-subtext">{isModule ? text.moduleOther : text.other}</h3>
                            <label class="flex min-h-10 items-center gap-2 rounded-md border border-darkborderc bg-bgcolor px-2.5 text-subtext focus-within:border-borderc">
                                <SearchIcon size={16} aria-hidden="true" />
                                <input type="search" class="min-w-0 flex-1 bg-transparent text-sm text-textcolor outline-hidden" bind:value={otherQuery} placeholder={text.otherPlaceholder} aria-label={isModule ? text.moduleOther : text.other} />
                            </label>
                            {#if chosenOther}
                                <p class="text-sm">✓ {chosenOther.name || text.unnamed}</p>
                            {/if}
                            {#if otherQuery.trim()}
                                {#if otherMatches.length}
                                    <div class="flex flex-col" role="group" aria-label={isModule ? text.moduleOther : text.other}>
                                        {#each otherMatches as other (other.id)}
                                            <button type="button" aria-pressed={dialog.target === other.id} class="rounded-md px-2.5 py-1.5 text-left text-sm hover:bg-selected {dialog.target === other.id ? 'bg-selected' : ''}" disabled={dialog.busy} onclick={() => selectContentUpdateTarget(other.id)}>{other.name || text.unnamed}</button>
                                        {/each}
                                    </div>
                                {:else}
                                    <p class="text-sm text-subtext">{text.noMatches}</p>
                                {/if}
                            {/if}
                        </div>
                    {/if}
                {:else if plan && counts}
                    {#if dialog.baseSource.kind === 'none'}
                        <div class="flex flex-col items-start gap-2 rounded-md border border-darkborderc bg-bgcolor p-3">
                            <p class="text-sm leading-relaxed">{text.noBase}</p>
                            <SettingButton variant="secondary" size="sm" disabled={dialog.busy} onclick={() => void loadContentUpdateBaseFile()}><FileUpIcon size={14} />{text.loadBaseFile}</SettingButton>
                        </div>
                    {:else if dialog.baseSource.kind === 'file'}
                        <div class="flex flex-wrap items-baseline gap-3 text-sm">
                            <span>{text.baseFromFile(dialog.baseSource.name ?? '')}</span>
                            <button type="button" class="text-subtext underline underline-offset-2 hover:text-textcolor" disabled={dialog.busy} onclick={() => void loadContentUpdateBaseFile()}>{text.loadOtherBaseFile}</button>
                        </div>
                    {/if}

                    {#if plan.items.length}
                        <ul class="flex flex-wrap gap-1.5">
                            {#each summaryKeys as key}
                                {#if counts[key]}<li class="rounded-full border border-darkborderc px-2.5 py-0.5 text-xs {key === 'conflict' || key === 'differs' ? 'text-danger-400' : 'text-subtext'}">{text.summary[key]} <b class="text-textcolor tabular-nums">{counts[key]}</b></li>{/if}
                            {/each}
                            {#if editCount}<li class="rounded-full border border-darkborderc px-2.5 py-0.5 text-xs text-subtext">{text.summaryEdited} <b class="text-textcolor tabular-nums">{editCount}</b></li>{/if}
                        </ul>

                        <div class="flex flex-wrap items-center gap-2">
                            <label class="flex min-h-9 min-w-0 flex-[1_1_14rem] items-center gap-2 rounded-md border border-darkborderc bg-bgcolor px-2.5 text-subtext focus-within:border-borderc">
                                <SearchIcon size={16} aria-hidden="true" />
                                <input type="search" class="min-w-0 flex-1 bg-transparent text-sm text-textcolor outline-hidden" bind:value={query} placeholder={text.searchPlaceholder} aria-label={text.searchPlaceholder} />
                                {#if query}<button type="button" class="rounded-sm p-0.5 hover:text-textcolor" aria-label={text.clearSearch} onclick={() => { query = '' }}><XIcon size={14} /></button>{/if}
                            </label>
                            <span class="inline-flex shrink-0 overflow-hidden rounded-md border border-darkborderc" role="group" aria-label={text.viewLabel}>
                                <button type="button" aria-pressed={!dialog.showAll} class="{segment} {!dialog.showAll ? segmentOn : ''}" onclick={() => { dialog.showAll = false }}>{text.viewDecide(toDecide.length)}</button>
                                <button type="button" aria-pressed={dialog.showAll} class="{segment} border-l border-darkborderc {dialog.showAll ? segmentOn : ''}" onclick={() => { dialog.showAll = true }}>{text.viewAll(plan.items.length)}</button>
                            </span>
                        </div>

                        <div class="flex flex-wrap items-center justify-between gap-2">
                            {#if toDecide.length}
                                <strong class="text-sm tabular-nums {counts.unresolved === 0 ? 'text-success-400' : 'text-danger-400'}">{text.decideCount(counts.unresolved)}</strong>
                            {:else}
                                <span class="text-sm text-subtext">{text.nothingToDecide}</span>
                            {/if}
                            {#if visible.length}
                                <span class="inline-flex items-center gap-2">
                                    <span class="text-xs text-subtext">{query.trim() ? text.bulkFound(visible.length) : text.bulkShown(visible.length)}</span>
                                    {@render scopeButtons(visible, query.trim() ? text.bulkFound(visible.length) : text.bulkShown(visible.length))}
                                </span>
                            {/if}
                        </div>

                        {#if !visible.length && query.trim()}
                            <div class="flex flex-col items-center gap-1 py-3">
                                <p class="text-sm text-subtext">{text.noMatches}</p>
                                {#if hiddenMatches}<button type="button" class="text-sm underline underline-offset-2" onclick={() => { dialog.showAll = true }}>{text.showHiddenMatches(hiddenMatches)}</button>{/if}
                            </div>
                        {/if}

                        {#each sections as section (section.id)}
                            {@const sectionKey = `g:${section.id}`}
                            {@const sectionShown = isOpen(sectionKey, section.items.length)}
                            {@const left = unresolvedIn(section.items)}
                            <section class="flex flex-col gap-1 border-t border-darkborderc pt-1.5">
                                <div class="flex flex-wrap items-center justify-between gap-1.5">
                                    <button type="button" class="flex min-h-9 items-center gap-2 rounded-md px-1.5 py-1 text-left hover:bg-selected" aria-expanded={sectionShown} onclick={() => toggleSection(sectionKey, section.items.length)}>
                                        <ChevronRightIcon size={16} class="shrink-0 text-subtext transition-transform {sectionShown ? 'rotate-90' : ''}" />
                                        <span class="font-semibold">{text.groups[section.id]}</span>
                                        <span class="text-xs text-subtext tabular-nums">{section.items.length}</span>
                                        {#if left}<span class="text-xs text-danger-400 tabular-nums">{text.leftCount(left)}</span>{/if}
                                    </button>
                                    {@render scopeButtons(section.items, text.groups[section.id])}
                                </div>
                                {#if sectionShown}
                                    {#each section.subs as sub (sub.id)}
                                        {@const label = subLabel(section, sub)}
                                        {#if label && (section.subs.length > 1 || sub.id.startsWith('folder:'))}
                                            {@const subKey = `s:${section.id}:${sub.id}`}
                                            {@const subShown = isOpen(subKey, sub.items.length)}
                                            {@const subLeft = unresolvedIn(sub.items)}
                                            <div class="ml-4 flex flex-col gap-1">
                                                <div class="flex flex-wrap items-center justify-between gap-1.5">
                                                    <button type="button" class="flex min-h-9 items-center gap-2 rounded-md px-1.5 py-1 text-left text-sm hover:bg-selected" aria-expanded={subShown} onclick={() => toggleSection(subKey, sub.items.length)}>
                                                        <ChevronRightIcon size={14} class="shrink-0 text-subtext transition-transform {subShown ? 'rotate-90' : ''}" />
                                                        <span class="font-medium">{label}</span>
                                                        <span class="text-xs text-subtext tabular-nums">{sub.items.length}</span>
                                                        {#if subLeft}<span class="text-xs text-danger-400 tabular-nums">{text.leftCount(subLeft)}</span>{/if}
                                                    </button>
                                                    {@render scopeButtons(sub.items, label)}
                                                </div>
                                                {#if subShown}
                                                    <ul class="ml-4 flex flex-col">{#each sub.items as item (item.id)}{@render itemView(item)}{/each}</ul>
                                                {/if}
                                            </div>
                                        {:else}
                                            <ul class="ml-4 flex flex-col">{#each sub.items as item (item.id)}{@render itemView(item)}{/each}</ul>
                                        {/if}
                                    {/each}
                                {/if}
                            </section>
                        {/each}
                    {:else}
                        <p class="text-sm text-subtext">{isModule ? text.moduleNoChanges : text.noChanges}</p>
                    {/if}

                    {#if greetingChanged && !isModule}<p class="text-sm text-subtext">{text.greetingNote}</p>{/if}
                    <p class="border-t border-darkborderc pt-2 text-xs text-subtext">{isModule ? text.moduleBackupNote : text.backupNote}</p>
                    {#if dialog.targetBusy}<p class="text-sm text-danger-400" role="alert">{text.busy}</p>{/if}
                {/if}
                {#if dialog.error}<p class="text-sm text-danger-400" role="alert">{dialog.error}</p>{/if}
            </div>

            <footer class="flex flex-wrap justify-end gap-2 border-t border-darkborderc px-5 py-3">
                {#if dialog.step === 'choose'}
                    <SettingButton variant="secondary" disabled={dialog.busy} onclick={() => chooseContentUpdate('cancel')}>{text.cancel}</SettingButton>
                    <SettingButton variant="secondary" disabled={dialog.busy} onclick={() => chooseContentUpdate('new')}>{text.installNew}</SettingButton>
                    <SettingButton busy={dialog.busy} disabled={!dialog.target} onclick={() => void reviewContentUpdate()}>{text.next}</SettingButton>
                {:else}
                    <!-- No "install as new" here: it would sit where "Next" was, so a double click would pick it. -->
                    <SettingButton variant="secondary" disabled={dialog.busy} onclick={() => backToContentUpdateTargets()}>{text.back}</SettingButton>
                    <SettingButton busy={dialog.busy} disabled={!counts || counts.unresolved > 0} onclick={() => chooseContentUpdate('update')}>{text.update}</SettingButton>
                {/if}
            </footer>
        </div>
    </div>
{/if}
