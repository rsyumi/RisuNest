// Standalone synthetic UI fixture. No application bootstrap or stored user data.
import { mount } from 'svelte'
import '../../../src/styles.css'
import NativeFileJobDialog from '../../../src/lib/Others/NativeFileJobDialog.svelte'
import { nativeFileOperation } from '../../../src/ts/storage/nativeFileJobManager'
import { emptyNativeImportCounts } from '../../../src/ts/storage/nativeFileJobs'

nativeFileOperation.set({
    kind: 'import',
    presentation: 'dialog',
    format: 'content',
    startedAt: Date.now() - 12_000,
    source: { name: 'synthetic-library.charx', bytes: 500 * 1024 * 1024 },
    observedStages: ['reading-archive', 'preparing-attachments'],
    blocking: false,
    cancelRequested: false,
    partialWritesPossible: false,
    status: {
        jobId: 'synthetic',
        kind: 'prepare-content-import',
        state: 'running',
        phase: 'reading-source',
        progress: { completedBytes: 0, completedItems: 1250 },
        detail: {
            stage: 'preparing-attachments',
            stageUnit: 'items',
            stageCompleted: 1250,
            stageTotal: 2500,
            counts: {
                ...emptyNativeImportCounts(),
                assets: 1250,
                attachmentsPrepared: 1250,
            },
        },
    },
})
mount(NativeFileJobDialog, { target: document.getElementById('app')! })
