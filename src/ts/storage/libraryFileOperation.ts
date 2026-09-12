/** Admission only. The existing native file job manager owns execution/cancellation. */
let reservation: symbol | undefined;

export function isLibraryFileOperationReserved(): boolean {
  return reservation !== undefined;
}

export function reserveLibraryFileOperation(): () => void {
  if (reservation) throw new Error("library-file-operation-busy");
  const token = Symbol("library-file-operation");
  reservation = token;
  return () => {
    if (reservation === token) reservation = undefined;
  };
}
