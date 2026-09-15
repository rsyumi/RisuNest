import {
    dataHealthDeepFraction,
    groupDataHealthFindings,
    isDataHealthCancellation,
    type DataHealthGroup,
    type DataHealthResult,
} from './dataHealth'

export type DataHealthRun = 'quick' | 'deep' | null

export interface DataHealthSnapshot {
    loading: boolean
    running: DataHealthRun
    /** A deep scan that stopped before it finished, so resuming is offered. */
    resumable: boolean
    result: DataHealthResult | null
    groups: DataHealthGroup[]
    deepFraction: number | null
    failed: boolean
}

export interface DataHealthDependencies {
    getResult(): Promise<DataHealthResult | null>
    scan(): Promise<DataHealthResult>
    deepScan(resume: boolean): Promise<DataHealthResult>
    cancel(): Promise<void>
}

function derive(
    result: DataHealthResult | null,
): Pick<DataHealthSnapshot, 'result' | 'groups' | 'deepFraction' | 'resumable'> {
    return {
        result,
        groups: result ? groupDataHealthFindings(result.items) : [],
        deepFraction: dataHealthDeepFraction(result),
        resumable: Boolean(
            result && result.depth === 'deep' && result.deep && !result.deep.complete,
        ),
    }
}

/**
 * Drives the diagnosis screen. A deep scan is a loop of bounded native pages, so a cancel takes
 * effect within one page and the progress it reports is what the screen shows.
 */
export function createDataHealthModel(deps: DataHealthDependencies) {
    let state: DataHealthSnapshot = {
        loading: false,
        running: null,
        failed: false,
        ...derive(null),
    }
    const listeners = new Set<(snapshot: DataHealthSnapshot) => void>()
    const update = (next: Partial<DataHealthSnapshot>) => {
        state = { ...state, ...next }
        listeners.forEach((listener) => listener(state))
    }
    let cancelRequested = false

    const finish = (result: DataHealthResult | null) =>
        update({ ...derive(result) })

    const runDeep = async (resume: boolean): Promise<void> => {
        let next = resume
        for (;;) {
            const result = await deps.deepScan(next)
            finish(result)
            if (result.deep?.complete || cancelRequested) return
            next = true
        }
    }

    const start = async (
        running: Exclude<DataHealthRun, null>,
        action: () => Promise<void>,
    ): Promise<void> => {
        if (state.running) return
        cancelRequested = false
        update({ running, failed: false })
        try {
            await action()
        } catch (error) {
            if (!isDataHealthCancellation(error)) {
                update({ failed: true })
                throw error
            }
        } finally {
            update({ running: null })
        }
    }

    return {
        snapshot: () => state,
        subscribe(listener: (snapshot: DataHealthSnapshot) => void) {
            listeners.add(listener)
            listener(state)
            return () => listeners.delete(listener)
        },
        /** Shows the last diagnosis without scanning again. */
        async load(): Promise<void> {
            if (state.loading || state.running) return
            update({ loading: true })
            try {
                finish(await deps.getResult())
            } catch {
                update({ failed: true })
            } finally {
                update({ loading: false })
            }
        },
        quickScan(): Promise<void> {
            return start('quick', async () => finish(await deps.scan()))
        },
        deepScan(resume: boolean): Promise<void> {
            return start('deep', () => runDeep(resume))
        },
        async cancel(): Promise<void> {
            if (!state.running) return
            cancelRequested = true
            await deps.cancel()
        },
    }
}
