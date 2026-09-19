/** Admission only. The existing native file job manager owns execution/cancellation. */
let reservation: symbol | undefined;
const releasedListeners = new Set<() => void>();

export function subscribeLibraryFileOperationReleased(listener: () => void): () => void {
  releasedListeners.add(listener);
  return () => { releasedListeners.delete(listener); };
}

export function isLibraryFileOperationReserved(): boolean {
  return reservation !== undefined;
}

export function reserveLibraryFileOperation(): () => void {
  if (reservation) throw new Error("library-file-operation-busy");
  const token = Symbol("library-file-operation");
  reservation = token;
  return () => {
    if (reservation !== token) return;
    reservation = undefined;
    for (const listener of releasedListeners) {
      // A scheduling notification must not undo a settled file operation.
      try { listener(); } catch {}
    }
  };
}
