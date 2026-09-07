import { downloadFile, globalFetch } from "./globalApi.svelte";

let bgmElement: HTMLAudioElement | null = null
let stopDomObservation: (() => void) | undefined

function nodeObserve(
    node: HTMLElement,
    signal: AbortSignal,
    bound: WeakSet<HTMLElement>,
    restartBgm: () => void,
) {
    const hlLang = node.getAttribute('x-hl-lang')
    const ctrlName = node.getAttribute('risu-ctrl')

    if (hlLang && !bound.has(node)) {
        bound.add(node)
        node.addEventListener(
            'contextmenu',
            (e) => {
                const currentLanguage = node.getAttribute('x-hl-lang')
                if (!currentLanguage) return
                e.preventDefault()

                const prevContextMenu =
                    document.getElementById('code-contextmenu')
                if (prevContextMenu) {
                    prevContextMenu.remove()
                }

                const menu = document.createElement('div')
                menu.id = 'code-contextmenu'
                menu.setAttribute(
                    'class',
                    'fixed z-50 min-w-[160px] py-2 bg-gray-800 rounded-lg border border-gray-700',
                )

                const copyOption = document.createElement('div')
                copyOption.textContent = 'Copy'
                copyOption.setAttribute(
                    'class',
                    'px-4 py-2 text-sm text-gray-300 hover:bg-gray-700 cursor-pointer',
                )
                copyOption.addEventListener('click', () => {
                    navigator.clipboard.writeText(node.textContent)
                    menu.remove()
                })

                const downloadOption = document.createElement('div')
                downloadOption.textContent = 'Download'
                downloadOption.setAttribute(
                    'class',
                    'px-4 py-2 text-sm text-gray-300 hover:bg-gray-700 cursor-pointer',
                )
                downloadOption.addEventListener('click', async () => {
                    const blob = new Blob([node.textContent], {
                        type: 'text/plain',
                    })
                    await downloadFile(
                        'code.' + currentLanguage,
                        new Uint8Array(await blob.arrayBuffer()),
                    )
                    menu.remove()
                })

                menu.appendChild(copyOption)
                menu.appendChild(downloadOption)

                menu.style.left = e.clientX + 'px'
                menu.style.top = e.clientY + 'px'

                document.body.appendChild(menu)
            },
            { signal },
        )
    }

    if (ctrlName) {
        const split = ctrlName.split('___')

        switch (split[0]) {
            case 'bgm': {
                const volume = split[1] === 'auto' ? 0.5 : parseFloat(split[1])
                if (!bgmElement) {
                    const audio = new Audio(split[2])
                    bgmElement = audio
                    audio.volume = volume
                    audio.addEventListener(
                        'ended',
                        () => {
                            if (bgmElement !== audio) return
                            audio.remove()
                            bgmElement = null
                            restartBgm()
                        },
                        { signal },
                    )
                    void audio.play().catch(() => {
                        if (bgmElement !== audio) return
                        audio.pause()
                        audio.remove()
                        bgmElement = null
                    })
                }
                break
            }
        }
    }
}

export function startObserveDom(): () => void {
    if (stopDomObservation) return stopDomObservation
    const controller = new AbortController()
    const bound = new WeakSet<HTMLElement>()
    const restartBgm = () => {
        if (controller.signal.aborted) return
        document.querySelectorAll<HTMLElement>('[risu-ctrl]').forEach(visit)
    }
    const visit = (node: HTMLElement) => {
        if (node.isConnected)
            nodeObserve(node, controller.signal, bound, restartBgm)
    }
    const visitSubtree = (node: Node) => {
        if (!(node instanceof HTMLElement)) return
        visit(node)
        node.querySelectorAll<HTMLElement>('[x-hl-lang], [risu-ctrl]').forEach(
            visit,
        )
    }
    const observer = new MutationObserver((mutations) => {
        for (const mutation of mutations) {
            if (mutation.type === 'attributes')
                visit(mutation.target as HTMLElement)
            else mutation.addedNodes.forEach(visitSubtree)
        }
    })
    observer.observe(document.body, {
        childList: true,
        subtree: true,
        attributes: true,
        attributeFilter: ['x-hl-lang', 'risu-ctrl'],
    })
    visitSubtree(document.body)
    const closeMenu = () =>
        document.getElementById('code-contextmenu')?.remove()
    document.addEventListener('click', closeMenu, { signal: controller.signal })
    const stop = () => {
        if (controller.signal.aborted) return
        controller.abort()
        observer.disconnect()
        closeMenu()
        bgmElement?.pause()
        bgmElement?.remove()
        bgmElement = null
        stopDomObservation = undefined
    }
    window.addEventListener('pagehide', stop, { signal: controller.signal })
    stopDomObservation = stop
    return stop
}

let claudeObserverRunning = false;
let lastClaudeObserverLoad = 0;
let lastClaudeRequestTimes = 0;
let lastClaudeObserverPayload:any = null;
let lastClaudeObserverHeaders:any = null;
let lastClaudeObserverURL:any = null;

export function registerClaudeObserver(arg:{
    url:string,
    body:any,
    headers:any,
}) {
    lastClaudeRequestTimes = 0;
    lastClaudeObserverLoad = Date.now();
    lastClaudeObserverPayload = safeStructuredClone(arg.body)
    lastClaudeObserverHeaders = arg.headers;
    lastClaudeObserverURL = arg.url;
    lastClaudeObserverPayload.max_tokens = 10;
    claudeObserver()
}

function claudeObserver(){
    if(claudeObserverRunning){
        return
    }
    claudeObserverRunning = true;

    const fetchIt = async (tries = 0)=>{
        const res = await globalFetch(lastClaudeObserverURL, {
            body: lastClaudeObserverPayload,
            headers: lastClaudeObserverHeaders,
            method: "POST"
        })
        if(res.status >= 400){
            if(tries < 3){
                fetchIt(tries + 1)
            }
        }
    }

    const func = ()=>{       
        //request every 4 minutes and 30 seconds
        if(lastClaudeObserverLoad > Date.now() - 1000 * 60 * 4.5){
            return
        }
        
        if(lastClaudeRequestTimes > 4){
            return
        }
        fetchIt()
        lastClaudeObserverLoad = Date.now();
        lastClaudeRequestTimes += 1;
    }
    
    setInterval(func, 20000)
}
