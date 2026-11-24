import {
    ConversationNotFoundError,
    ConversationSessionInactiveError,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
} from '../storage/activeConversationSession'
import { PersistentMutationFencedError } from '../storage/saveCoordinator'

/** Errors that end a script because its conversation was left or changed, not because the script failed. */
export function isConversationStop(error: unknown): boolean {
    return error instanceof ConversationSessionInactiveError
        || error instanceof ConversationSessionStaleError
        || error instanceof ConversationNotFoundError
        || error instanceof MessageLocatorMismatchError
        || error instanceof PersistentMutationFencedError
}
