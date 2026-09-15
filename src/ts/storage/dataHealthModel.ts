import {
    dataHealthDeepFraction,
    groupDataHealthFindings,
    isDataHealthCancellation,
    preferredRepairSelection,
    toggleRepairSelection,
    type DataHealthGroup,
    type DataHealthResult,
    type RepairCandidate,
    type RepairJournalSummary,
    type RepairPreview,
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
    /** What the diagnosis can be answered with, and what the reader has chosen. */
    candidates: RepairCandidate[]
    selection: string[]
    preview: RepairPreview | null
    /** Repairs that can still be undone, newest first. */
    journals: RepairJournalSummary[]
    /** Records an undo left alone because they changed after the repair. */
    skipped: string[]
    repairing: boolean
}

export interface DataHealthDependencies {
    getResult(): Promise<DataHealthResult | null>
    scan(): Promise<DataHealthResult>
    deepScan(resume: boolean): Promise<DataHealthResult>
    cancel(): Promise<void>
    planRepair(): Promise<RepairCandidate[]>
    previewRepair(selection: string[]): Promise<RepairPreview>
    applyRepair(
        selection: string[],
        snapshot: boolean,
    ): Promise<{ result: DataHealthResult }>
    listJournals(): Promise<RepairJournalSummary[]>
    undoRepair(
        journalId: string,
    ): Promise<{ result: DataHealthResult; skipped: string[] }>
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
        candidates: [],
        selection: [],
        preview: null,
        journals: [],
        skipped: [],
        repairing: false,
        ...derive(null),
    }
    const listeners = new Set<(snapshot: DataHealthSnapshot) => void>()
    const update = (next: Partial<DataHealthSnapshot>) => {
        state = { ...state, ...next }
        listeners.forEach((listener) => listener(state))
    }
    let cancelRequested = false

    const finish = (result: DataHealthResult | null) =>
        update({ ...derive(result), candidates: [], selection: [], preview: null })

    /** Reads what the current diagnosis can be answered with, and preselects the fixed choices. */
    const loadPlan = async (): Promise<void> => {
        if (!state.result || state.result.items.length === 0) return
        const candidates = await deps.planRepair()
        const selection = preferredRepairSelection(candidates)
        update({ candidates, selection })
        await refreshPreview(selection)
    }

    const refreshPreview = async (selection: string[]): Promise<void> => {
        update({
            preview:
                selection.length > 0 ? await deps.previewRepair(selection) : null,
        })
    }

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
        /** Reads the repair choices for the diagnosis on screen. */
        async loadRepairs(): Promise<void> {
            if (state.repairing || state.running) return
            try {
                await loadPlan()
                update({ journals: await deps.listJournals() })
            } catch {
                update({ failed: true })
            }
        },
        /** One choice per finding: picking one drops the other answers to the same finding. */
        async toggle(id: string): Promise<void> {
            const selection = toggleRepairSelection(
                state.candidates,
                state.selection,
                id,
            )
            update({ selection })
            try {
                await refreshPreview(selection)
            } catch {
                update({ failed: true })
            }
        },
        async apply(snapshot: boolean): Promise<void> {
            if (state.repairing || state.selection.length === 0) return
            update({ repairing: true, failed: false, skipped: [] })
            try {
                const applied = await deps.applyRepair(state.selection, snapshot)
                finish(applied.result)
                await loadPlan()
                update({ journals: await deps.listJournals() })
            } catch (error) {
                update({ failed: true })
                throw error
            } finally {
                update({ repairing: false })
            }
        },
        async undo(journalId: string): Promise<void> {
            if (state.repairing) return
            update({ repairing: true, failed: false, skipped: [] })
            try {
                const undone = await deps.undoRepair(journalId)
                finish(undone.result)
                await loadPlan()
                update({
                    journals: await deps.listJournals(),
                    skipped: undone.skipped,
                })
            } catch (error) {
                update({ failed: true })
                throw error
            } finally {
                update({ repairing: false })
            }
        },
    }
}
