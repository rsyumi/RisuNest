export const compareVersions = (v1: string, v2: string): 0|1|-1 => {
    const v1parts = v1.split('.').map(Number);
    const v2parts = v2.split('.').map(Number);
    const len = Math.max(v1parts.length, v2parts.length);
    for (let i = 0; i < len; i++) {
        const part1 = v1parts[i] || 0;
        const part2 = v2parts[i] || 0;
        if (part1 > part2) return 1;
        if (part1 < part2) return -1;
    }
    return 0;
}

export interface PluginHeaderLink {
    link: string
    hoverText?: string
}

/** What a plugin file declares in its `//@` lines. */
export interface PluginHeader {
    /** The last `//@name`, or '' when the file has none. */
    name: string
    displayName?: string
    /** The first supported `//@api` value; '2.0' when none is declared. */
    apiVersion: string
    arguments: { [key: string]: 'int' | 'string' }
    argMeta: { [key: string]: { [key: string]: string } }
    customLink: PluginHeaderLink[]
    updateURL: string
    /** The plugin's own `//@version`, not the API version. */
    versionOfPlugin: string
    allowedIPC: string[]
}

export interface PluginHeaderParse {
    header: PluginHeader
    /** The first problem that stops an install, with the plugin name known when it was found. */
    error?: { message: string; name: string }
}

/** Removes a UTF-8 BOM at the start of any line, as the plugin file picker always has. */
export function stripPluginBom(code: string): string {
    return code.replace(/^\uFEFF/gm, '')
}

const supportedApiVersions = ['2.0', '2.1', '3.0']

/**
 * Reads the header of a plugin file. Every line is read, and `error` keeps the first
 * problem in the order an install reports them.
 */
export function parsePluginHeader(code: string): PluginHeaderParse {
    const lines = code.split('\n')
    let name = ''
    for (const line of lines) {
        if (line.startsWith('//@name')) {
            name = line.slice(7).trim()
            break
        }
    }
    const header: PluginHeader = {
        name: '',
        displayName: undefined,
        apiVersion: '2.0',
        arguments: {},
        argMeta: {},
        customLink: [],
        updateURL: '',
        versionOfPlugin: '',
        allowedIPC: [],
    }
    let error: PluginHeaderParse['error']
    const fail = (message: string) => {
        error ??= { message, name }
    }

    for (const line of lines) {
        if (line.startsWith('//@name')) {
            const provied = line.slice(7)
            if (provied === '') {
                fail('plugin name must be longer than 0, did you put it correctly?')
            }
            else {
                name = provied.trim()
            }
        }
        if (line.startsWith('//@api')) {
            const proviedVersions = line.slice(6).trim().split(' ')
            for (const ver of proviedVersions) {
                if (supportedApiVersions.includes(ver)) {
                    header.apiVersion = ver
                    break
                }
                else {
                    console.warn(`Plugin API version "${ver}" is not supported.`)
                }
            }
        }
        if (line.startsWith('//@display-name')) {
            const provied = line.slice('//@display-name'.length + 1)
            if (provied === '') {
                fail('plugin display name must be longer than 0, did you put it correctly?')
            }
            else {
                header.displayName = provied.trim()
            }
        }

        if (line.startsWith('//@link')) {
            const link = line.split(' ')[1]
            if (!link || link === '') {
                fail('plugin link is empty, did you put it correctly?')
            }
            else if (!link.startsWith('https')) {
                fail('plugin link must start with https, did you check it?')
            }
            else {
                const hoverText = line.split(' ').slice(2).join(' ').trim()
                header.customLink.push({
                    link: link,
                    hoverText: hoverText || undefined
                })
            }
        }
        if (line.startsWith('//@risu-arg') || line.startsWith('//@arg')) {
            const provied = line.trim().split(' ')
            const provKey = provied[1]
            if (provied.length < 3) {
                fail('plugin argument is incorrect, did you put space in argument name?')
            }
            else if (provied[2] !== 'int' && provied[2] !== 'string') {
                fail(`plugin argument type is "${provied[2]}", which is an unknown type.`)
            }
            else {
                header.arguments[provKey] = provied[2]

                if (provied.length > 3) {
                    const meta: {[key:string]:string} = {}
                    //Compatibility layer for unofficial meta
                    let metaStr = provied.slice(3).join(' ').replace(
                        /{{(.+?)(::?(.+?))?}}/g,
                        (a,g1:string,g2,g3:string) => {
                            console.log(g1,g3)
                            meta[g1] = g3 || '1'
                            return ''
                        }
                    ).trim()

                    if (metaStr) {
                        meta['description'] = metaStr
                    }

                    header.argMeta[provKey] = meta
                }
            }
        }

        if (line.startsWith('//@update-url')) {
            header.updateURL = line.split(' ')[1] ?? ''

            try {
                const url = new URL(header.updateURL)
                if (url.protocol !== 'https:') {
                    fail('plugin update URL must start with https, did you put it correctly?')
                }
            } catch {
                fail('plugin update URL is not a valid URL, did you put it correctly?')
            }
        }

        if (line.startsWith('//@version')) {
            header.versionOfPlugin = line.split(' ').slice(1).join(' ').trim()

            const versionLocation = code.indexOf('//@version')
            const numberOfBytesBefore = new TextEncoder().encode(code.slice(0, versionLocation) + line).length
            if (numberOfBytesBefore > 500) {
                fail('plugin version declaration must be within the first 512 Bytes of the file for proper parsing. move //@version line to the top of the file.')
            }
        }

        if (line.startsWith('//@allowed-ipc')) {
            const provied = line.trim().split(' ')
            if (provied.length < 2) {
                fail('plugin allowed IPC declaration is incorrect, did you put space after //@allowed-ipc?')
            }
            else {
                header.allowedIPC.push(...provied.slice(1))
            }
        }
    }
    header.name = name

    if (name.length === 0) {
        fail('plugin name not found, did you put it correctly?')
    }
    if (name === '*') {
        fail('plugin name cannot be "*".')
    }
    if (header.updateURL && header.versionOfPlugin.length === 0) {
        fail('plugin version not found, did you put it correctly? It is required when update URL is provided.')
    }
    if (header.versionOfPlugin && compareVersions(header.versionOfPlugin, '0.0.1') === -1) {
        fail('plugin version must be at least 0.0.1')
    }

    return { header, error }
}
