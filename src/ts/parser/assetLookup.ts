export interface AssetPath {
    srcPaths: string[]
    ext?: string
}

type AssetTuple = readonly string[]

interface FuzzyAssetEntry {
    exactName: string
    normalized: string
    order: number
    value: AssetPath
}

export interface AssetLookupIndex {
    additionalExact: Map<string, AssetPath>
    emotionExact: Map<string, AssetPath>
    characterEntriesByLength: Map<number, FuzzyAssetEntry[]>
}

const assetExtensions = ['webp', 'png', 'jpg', 'jpeg', 'gif', 'mp4', 'webm', 'avi', 'm4p', 'm4v', 'mp3', 'wav', 'ogg']

function normalizeExactName(name: string): string {
    return name.toLocaleLowerCase()
}

export function normalizeFuzzyAssetName(name: string): string {
    let normalized = normalizeExactName(name)
    for (const extension of assetExtensions) {
        if (normalized.endsWith(`.${extension}`)) {
            normalized = normalized.substring(0, normalized.length - extension.length - 1)
        }
    }
    return normalized.trim().replace(/[_ -.]/g, '')
}

function addExactAssets(exact: Map<string, AssetPath>, assets: readonly AssetTuple[]): void {
    for (const asset of assets) {
        const key = normalizeExactName(asset[0])
        const existing = exact.get(key)
        if (!existing) {
            exact.set(key, { srcPaths: [asset[1]], ext: asset[2] })
        }
        else if (existing.ext === asset[2]) {
            existing.srcPaths.push(asset[1])
        }
    }
}

export function createAssetLookupIndex(input: {
    characterAssets: readonly AssetTuple[]
    moduleAssets: readonly AssetTuple[]
    emotionAssets: readonly AssetTuple[]
}): AssetLookupIndex {
    const additionalExact = new Map<string, AssetPath>()
    addExactAssets(additionalExact, input.characterAssets)
    addExactAssets(additionalExact, input.moduleAssets)
    const emotionExact = new Map<string, AssetPath>()
    for (const emotion of input.emotionAssets) {
        emotionExact.set(normalizeExactName(emotion[0]), { srcPaths: [emotion[1]] })
    }

    const characterEntriesByLength = new Map<number, FuzzyAssetEntry[]>()
    input.characterAssets.forEach((asset, order) => {
        const normalized = normalizeFuzzyAssetName(asset[0])
        const entry = {
            exactName: normalizeExactName(asset[0]),
            normalized,
            order,
            value: { srcPaths: [asset[1]], ext: asset[2] },
        }
        const bucket = characterEntriesByLength.get(normalized.length)
        if (bucket) {
            bucket.push(entry)
        }
        else {
            characterEntriesByLength.set(normalized.length, [entry])
        }
    })

    return { additionalExact, emotionExact, characterEntriesByLength }
}

export function resolveAdditionalAsset(
    index: AssetLookupIndex,
    name: string,
    maxDifference: number,
    distance: (left: string, right: string) => number = getAssetDistance,
): AssetPath | null {
    const exactName = normalizeExactName(name)
    const exact = index.additionalExact.get(exactName)
    if (exact) return exact
    if (index.characterEntriesByLength.size === 0 || maxDifference < 0) return null

    const target = normalizeFuzzyAssetName(exactName)
    let closest: FuzzyAssetEntry | null = null
    let closestDistance = maxDifference + 1
    const candidates: FuzzyAssetEntry[] = []

    const minimumLength = Math.max(0, Math.ceil(target.length - maxDifference))
    const maximumLength = Math.floor(target.length + maxDifference)
    for (let length = minimumLength; length <= maximumLength; length++) {
        const bucket = index.characterEntriesByLength.get(length)
        if (bucket) candidates.push(...bucket)
    }
    candidates.sort((left, right) => left.order - right.order)

    for (const candidate of candidates) {
        const candidateDistance = distance(target, candidate.normalized)
        if (candidateDistance < closestDistance) {
            closest = candidate
            closestDistance = candidateDistance
        }
    }

    if (!closest || closestDistance > maxDifference) return null
    index.additionalExact.set(closest.exactName, closest.value)
    return closest.value
}

export function resolveEmotionAsset(index: AssetLookupIndex, name: string): AssetPath | null {
    return index.emotionExact.get(normalizeExactName(name)) ?? null
}

export function getAssetDistance(left: string, right: string): number {
    const height = left.length + 1
    const width = right.length + 1
    const distances = new Int16Array(height * width)
    for (let row = 0; row < height; row++) {
        distances[row * width] = row
    }
    for (let column = 0; column < width; column++) {
        distances[column] = column
    }
    for (let row = 1; row < height; row++) {
        for (let column = 1; column < width; column++) {
            distances[row * width + column] = Math.min(
                distances[(row - 1) * width + column - 1] + (left.charAt(row - 1) === right.charAt(column - 1) ? 0 : 1),
                distances[(row - 1) * width + column] + 1,
                distances[row * width + column - 1] + 1,
            )
        }
    }
    return distances[height * width - 1]
}
