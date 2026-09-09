import { Mutex } from '../mutex'
import { getCurrentChat, type Chat } from '../storage/database.svelte'
import { peekActiveConversationSession } from '../storage/persistentDataRuntime.svelte'
import {
    ConversationSessionInactiveError,
    requireCurrentConversationSession,
    type ActiveConversationSession,
} from '../storage/activeConversationSession'

const userTriggerMutexes = new WeakMap<ActiveConversationSession, Mutex>()

/** Queue the entire user action, including operation creation and commit. */
export function runSerializedUserTrigger<T>(
    characterId: string,
    chat: Chat,
    run: () => Promise<T>,
): Promise<T> {
    const session = peekActiveConversationSession()
    if (!session?.matchesConversation(characterId, chat)) return run()
    // The UI working set may expose a Svelte proxy for the session-owned chat.
    const selectedChat = getCurrentChat()

    let mutex = userTriggerMutexes.get(session)
    if (!mutex) {
        mutex = new Mutex()
        userTriggerMutexes.set(session, mutex)
    }
    return mutex.runExclusive(async () => {
        requireCurrentConversationSession(
            session,
            peekActiveConversationSession(),
        )
        if (
            getCurrentChat() !== selectedChat ||
            !session.matchesConversation(characterId, chat)
        ) {
            throw new ConversationSessionInactiveError()
        }
        // Earlier clicks may have committed while waiting. Capture the operation
        // in run(), against that latest state, only after checking its owner.
        return run()
    })
}
