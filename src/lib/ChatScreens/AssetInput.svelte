<script lang="ts">
    import ListPager from "src/lib/UI/GUI/ListPager.svelte"
    import { FileMusicIcon, PlusIcon } from "@lucide/svelte";
    import { type character, type groupChat } from "src/ts/storage/database.svelte";
    import { getFileSrc, saveAsset } from "src/ts/globalApi.svelte";
    import { selectMultipleFile } from "src/ts/util";
    interface Props {
        currentCharacter: character|groupChat;
        onSelect: (additionalAsset:[string,string,string])=>void;
    }

    const { currentCharacter, onSelect }: Props = $props();

    let assetFileExtensions:string[] = $state([])
    let assetFilePath:string[] = $state([])

    let assetPage = $state(0)
    const allAssets = $derived(currentCharacter.type === 'character' ? currentCharacter.additionalAssets ?? [] : [])
    const assetRows = $derived(allAssets.slice(assetPage * 60, (assetPage + 1) * 60).map((asset, offset) => ({ asset, i: assetPage * 60 + offset })))
    $effect(() => {
        let active = true
        assetFilePath = []
        const extensions: string[] = []
        for (const { asset, i } of assetRows) {
            extensions[i] = asset[2] || asset[1].split('.').pop()
            void getFileSrc(asset[1]).then(path => { if (active) assetFilePath[i] = path }).catch(() => {})
        }
        assetFileExtensions = extensions
        return () => { active = false }
    })

</script>
{#if currentCharacter.type ==='character'}
    <button class="hover:text-green-500 bg-textcolor2 flex justify-center items-center w-16 h-16 m-1 rounded-md" onclick={async () => {
        if(currentCharacter.type === 'character'){
            const da = await selectMultipleFile(['png', 'webp', 'mp4', 'mp3', 'gif'])
            currentCharacter.additionalAssets = currentCharacter.additionalAssets ?? []
            if(!da){
                return
            }
            for(const f of da){
                console.log(f)
                const img = f.data
                const name = f.name
                const extension = name.split('.').pop().toLowerCase()
                const imgp = await saveAsset(img,'',extension)
                currentCharacter.additionalAssets.push([name, imgp, extension])
            }
        }
    }}>
        <PlusIcon />
    </button>
    <ListPager bind:page={assetPage} total={allAssets.length} />
    {#if currentCharacter.additionalAssets}
        {#each assetRows as { asset: additionalAsset, i } (additionalAsset)}
                <button onclick={()=>{
                    onSelect(additionalAsset)
                }}>
                    {#if assetFilePath[i]}
                        {#if assetFileExtensions[i] === 'mp4'}
                            <!-- svelte-ignore a11y_media_has_caption -->
                            <video class="w-16 h-16 m-1 rounded-md"><source src={assetFilePath[i]} type="video/mp4"></video>
                        {:else if assetFileExtensions[i] === 'mp3'}
                            <div class='w-16 h-16 m-1 rounded-md bg-slate-500 flex flex-col justify-center items-center'>
                                <FileMusicIcon/>
                                <div class='w-16 px-1 text-ellipsis whitespace-nowrap overflow-hidden'>{additionalAsset[0]}</div>
                            </div>
                            <!-- <audio controls class="w-16 h-16 m-1 rounded-md"><source src={assetPath} type="audio/mpeg"></audio> -->
                        {:else}
                        <img src={assetFilePath[i]} class="w-16 h-16 m-1 rounded-md" alt={additionalAsset[0]}/>
                        {/if}
                    {/if}
                </button>
        {/each}
    {/if}
{/if}
