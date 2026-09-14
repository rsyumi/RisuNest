import { expect, it, vi } from 'vitest'
import {
    renameBookmarkWithFeedback,
    reportFailedBookmarkOperation,
} from './bookmarkOperation'

it('reports a bookmark that became stale before the rename prompt opened', async () => {
    const requestName = vi.fn()
    const reportFailure = vi.fn()

    await expect(renameBookmarkWithFeedback(
        async () => false,
        requestName,
        reportFailure,
    )).resolves.toBe(false)
    expect(requestName).not.toHaveBeenCalled()
    expect(reportFailure).toHaveBeenCalledOnce()
})

it('does not report an empty rename cancelled by the user', async () => {
    const reportFailure = vi.fn()

    await expect(renameBookmarkWithFeedback(
        async (requestName) => {
            await requestName('Current name')
            return false
        },
        async () => '',
        reportFailure,
    )).resolves.toBe(false)
    expect(reportFailure).not.toHaveBeenCalled()
})

it('reports an unsuccessful bookmark deletion', async () => {
    const reportFailure = vi.fn()

    await expect(reportFailedBookmarkOperation(
        async () => false,
        reportFailure,
    )).resolves.toBe(false)
    expect(reportFailure).toHaveBeenCalledOnce()
})

it('reports rejected rename and remove operations', async () => {
    const reportRenameFailure = vi.fn()
    const reportRemoveFailure = vi.fn()

    await expect(renameBookmarkWithFeedback(
        async () => { throw new Error('rename failed') },
        vi.fn(),
        reportRenameFailure,
    )).resolves.toBe(false)
    await expect(reportFailedBookmarkOperation(
        async () => { throw new Error('remove failed') },
        reportRemoveFailure,
    )).resolves.toBe(false)

    expect(reportRenameFailure).toHaveBeenCalledOnce()
    expect(reportRemoveFailure).toHaveBeenCalledOnce()
})
