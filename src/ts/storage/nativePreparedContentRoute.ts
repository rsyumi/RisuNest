import { contentMappingStatus } from "./contentImportOperation";
import {
  prepareNativeContentImport,
  syntheticNativeFileJobStatus,
  type NativeFileJobOptions,
  type NativeFileJobSource,
  type PreparedNativeContent,
  type PreparedNativeContentReceipt,
} from "./nativeFileJobs";

export interface NativePreparedContentRouteDependencies<TMapped, TResult> {
  prepare(
    source: NativeFileJobSource,
    displayName: string,
    options?: NativeFileJobOptions,
  ): Promise<PreparedNativeContentReceipt>;
  map(content: PreparedNativeContent): Promise<TMapped>;
  activate(
    mapped: TMapped,
    lifecycle: PreparedNativeContentReceipt,
    signal?: AbortSignal,
  ): Promise<TResult>;
  onCleanupWarning?(error: unknown): void;
}

const defaultPrepare: NativePreparedContentRouteDependencies<
  unknown,
  unknown
>["prepare"] = (source, displayName, options) =>
  prepareNativeContentImport(source, displayName, options);

export async function runNativePreparedContentRoute<TMapped, TResult>(
  source: NativeFileJobSource,
  displayName: string,
  dependencies: Omit<
    NativePreparedContentRouteDependencies<TMapped, TResult>,
    "prepare"
  > & {
    prepare?: NativePreparedContentRouteDependencies<
      TMapped,
      TResult
    >["prepare"];
  },
  options: NativeFileJobOptions = {},
): Promise<TResult> {
  let lastStatus: import("./nativeFileJobs").NativeFileJobStatus | undefined;
  const receipt = await (dependencies.prepare ?? defaultPrepare)(
    source,
    displayName,
    {
      ...options,
      onStatus: (status) => {
        lastStatus = status;
        options.onStatus?.(status);
      },
    },
  );
  let result: TResult;
  try {
    if (options.signal?.aborted)
      throw new DOMException("Native file job was cancelled", "AbortError");
    if (lastStatus) options.onStatus?.(contentMappingStatus(lastStatus));
    const mapped = await dependencies.map(receipt.content);
    if (options.signal?.aborted)
      throw new DOMException("Native file job was cancelled", "AbortError");
    const committing = () => {
      options.signal?.throwIfAborted();
      if (lastStatus)
        options.onStatus?.(
          syntheticNativeFileJobStatus(lastStatus, "activating"),
        );
    };
    const lifecycle: PreparedNativeContentReceipt = {
      ...receipt,
      prepareOwnerManifestAndSeal: async (bytes) => {
        committing();
        return receipt.prepareOwnerManifestAndSeal(bytes);
      },
      ...(receipt.sealPreparedContent
        ? {
            sealPreparedContent: async () => {
              committing();
              return receipt.sealPreparedContent!();
            },
          }
        : {}),
    };
    result = await dependencies.activate(mapped, lifecycle, options.signal);
    if (result === null) {
      await receipt.cancel();
      return result;
    }
  } catch (error) {
    try {
      await receipt.cancel();
    } catch {}
    throw error;
  }
  try {
    await receipt.confirmActivated();
  } catch (error) {
    try {
      dependencies.onCleanupWarning?.(error);
    } catch {}
  }
  return result;
}
