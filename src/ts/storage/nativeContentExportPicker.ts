export interface NativeContentExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

export interface NativeContentExportPickerDependencies {
  isDesktop(): boolean;
  isAndroid(): boolean;
  isIOS?(): boolean;
  runtime(): NativeContentExportRuntime;
}

export type NativeContentExportDestination =
  | { type: "desktopPath"; path: string }
  | { type: "androidSaf"; suggestedName: string }
  | { type: "iosFiles"; suggestedName: string };

export type NativeContentExportPickerFlow =
    /** Neither desktop nor Android: callers fall back to the compatibility path. */
    | { kind: 'unsupported' }
    /** The desktop save dialog was dismissed without choosing a destination. */
    | { kind: 'cancelled' }
    | {
        kind: 'ready'
        destination: NativeContentExportDestination
        expectedRevision: number
    }

/**
 * Shared platform-check/picker/flush/abort/revision flow for the native
 * content-export routes (character card, CharX, RISUM).
 */
export async function prepareNativeContentExportFromPicker(
    input: {
        suggestedName: string
        flushReason: string
        chooseDestination(): Promise<string | null>
    },
    options: { signal?: AbortSignal },
    dependencies: NativeContentExportPickerDependencies,
): Promise<NativeContentExportPickerFlow> {
    if (
      !dependencies.isDesktop() &&
      !dependencies.isAndroid() &&
      !dependencies.isIOS?.()
    )
      return { kind: "unsupported" };
    const destination = dependencies.isDesktop()
        ? await input.chooseDestination()
        : undefined
    if (dependencies.isDesktop() && !destination) return { kind: 'cancelled' }
    const runtime = dependencies.runtime()
    await runtime.flushPendingData(input.flushReason)
    if (options.signal?.aborted) {
        throw new DOMException('Native file job was cancelled', 'AbortError')
    }
    return {
      kind: "ready",
      destination: destination
        ? { type: "desktopPath", path: destination }
        : {
            type: dependencies.isIOS?.() ? "iosFiles" : "androidSaf",
            suggestedName: input.suggestedName,
          },
      expectedRevision: runtime.revision,
    };
}
