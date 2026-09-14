import type { AppUpdateState } from '../update/state.svelte'

export interface UpdateDialogModel {
    open: boolean
    busy: boolean
    canCancel: boolean
    percent: number | null
    primaryAction: 'install' | 'download' | 'open' | 'none'
}

export function buildUpdateDialogModel(state: AppUpdateState): UpdateDialogModel {
    const update = state.update
    const busy = state.phase === 'downloading' || state.phase === 'applying'
    const total = state.progress?.total ?? update?.downloadSize ?? 0
    const percent = state.progress && total > 0
        ? Math.min(100, Math.floor(state.progress.downloaded / total * 100))
        : null
    const primaryAction = !update ? 'none'
        : update.installStrategy === 'self-install' ? 'install'
            : update.installStrategy === 'stage-deb' ? 'download'
                : update.installStrategy === 'open-link' || update.installStrategy === 'disabled' ? 'open'
                    : 'none'
    return {
        open: state.popupVisible,
        busy,
        canCancel: state.phase === 'downloading',
        percent,
        primaryAction,
    }
}
