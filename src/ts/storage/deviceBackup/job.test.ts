// @vitest-environment node
import { describe, expect, it, vi } from "vitest";
import {
  createPortableExportIntentStore,
  rememberPortableExport,
  resumePendingPortableExport,
  type PortableExportResumeDependencies,
} from "./job";
const ios = vi.hoisted(() => ({ publish: vi.fn(), resume: vi.fn(), acknowledge: vi.fn() }));
vi.mock("../iosFiles", () => ({ exportIOSFile: ios.publish, getIOSPublication: ios.resume, acknowledgeIOSPublication: ios.acknowledge }));
import { AndroidSafDestinationError } from "../androidSafBridge";

function intentStore() {
  const values = new Map<string, string>();
  return {
    values,
    store: createPortableExportIntentStore({
      getItem: (key) => values.get(key) ?? null,
      setItem: (key, value) => {
        values.set(key, value);
      },
      removeItem: (key) => {
        values.delete(key);
      },
    }),
  };
}

const result = {
  revision: 3,
  sourceBytes: 1234,
  sourceSha256: "a".repeat(64),
  characterCount: 1,
  presetCount: 1,
  warningCodes: [],
  handoffPath: "/synthetic/export/backup.risunest",
};

function resumeFixture() {
  const { store, values } = intentStore();
  const invoke = vi.fn(async (command: string) => {
    if (command === "native_file_job_status")
      return {
        jobId: "synthetic-job",
        kind: "export-portable-backup",
        state: "succeeded",
        phase: "done",
        result,
      };
    if (command === "native_file_job_forget") return true;
    throw new Error("unexpected command");
  });
  const publishAndroid = vi.fn(async (intent) => ({
    requestId: intent.requestId,
    state: "succeeded" as const,
    bytes: 1234,
    warningCodes: [],
  }));
  const resumeAndroid = vi.fn(async (intent) => ({
    requestId: intent.requestId,
    state: "succeeded" as const,
    bytes: 1234,
    warningCodes: [],
  }));
  const dependencies: PortableExportResumeDependencies = {
    store,
    invoke: invoke as PortableExportResumeDependencies["invoke"],
    wait: async () => {},
    publishAndroid,
    resumeAndroid,
    acknowledgeAndroid: vi.fn(() => true),
    cleanupHandoff: vi.fn(async () => {}),
    onResult: vi.fn(),
  };
  return { store, values, dependencies, invoke, publishAndroid, resumeAndroid };
}

describe("portable file-job restart handoff", () => {
  it("persists iOS publication before acknowledgement and retries cleanup without republishing", async () => {
    const fixture = resumeFixture();
    rememberPortableExport("synthetic-job", { type: "iosFiles", suggestedName: "synthetic.risunest" }, fixture.store);
    ios.publish.mockResolvedValue({ bytes: 1234 });
    ios.acknowledge.mockImplementationOnce(async () => {
      expect(fixture.store.read()?.phase).toBe("published");
      throw new Error("synthetic acknowledgement failure");
    }).mockResolvedValue(undefined);
    await expect(resumePendingPortableExport(fixture.dependencies)).rejects.toMatchObject({ code: "cleanup-failed" });
    expect(fixture.dependencies.cleanupHandoff).not.toHaveBeenCalled();
    await expect(resumePendingPortableExport(fixture.dependencies)).resolves.toEqual(result);
    expect(ios.publish).toHaveBeenCalledOnce();
    expect(ios.acknowledge).toHaveBeenCalledTimes(2);
    expect(fixture.store.read()).toBeNull();
  });

  it("keeps export intent outside every plugin storage prefix", () => {
    const { values, store } = intentStore();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      store,
    );
    expect([...values.keys()]).toEqual(["risuNestPortableExportIntent"]);
    expect(store.read()).toMatchObject({
      jobId: "synthetic-job",
      publication: "android-saf",
      phase: "waiting-native",
    });
    expect(() =>
      rememberPortableExport("another-job", { type: "desktopPath" }, store),
    ).toThrow("Another portable export");
  });

  it("publishes after restart, checks bytes, acknowledges and cleans up before clearing intent", async () => {
    const fixture = resumeFixture();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      fixture.store,
    );
    await expect(
      resumePendingPortableExport(fixture.dependencies),
    ).resolves.toEqual(result);
    expect(fixture.publishAndroid).toHaveBeenCalledTimes(1);
    expect(fixture.resumeAndroid).not.toHaveBeenCalled();
    expect(fixture.dependencies.cleanupHandoff).toHaveBeenCalledExactlyOnceWith(
      result.handoffPath,
    );
    expect(fixture.invoke).toHaveBeenLastCalledWith("native_file_job_forget", {
      jobId: "synthetic-job",
    });
    expect(fixture.store.read()).toBeNull();
  });

  it("resumes an existing Android request without opening another picker", async () => {
    const fixture = resumeFixture();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      fixture.store,
    );
    fixture.store.write({
      ...fixture.store.read()!,
      phase: "publishing",
      requestId: "11111111-1111-4111-8111-111111111111",
    });
    await resumePendingPortableExport(fixture.dependencies);
    expect(fixture.publishAndroid).not.toHaveBeenCalled();
    expect(fixture.resumeAndroid).toHaveBeenCalledTimes(1);
  });

  it("retains the archive and intent when SAF byte verification fails", async () => {
    const fixture = resumeFixture();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      fixture.store,
    );
    fixture.publishAndroid.mockImplementationOnce(async (intent) => ({
      requestId: intent.requestId,
      state: "succeeded",
      bytes: 100,
      warningCodes: [],
    }));
    await expect(
      resumePendingPortableExport(fixture.dependencies),
    ).rejects.toMatchObject({ code: "destination-length-mismatch" });
    expect(fixture.dependencies.cleanupHandoff).not.toHaveBeenCalled();
    expect(fixture.store.read()?.phase).toBe("publishing");
  });

  it("distinguishes saved output from failed cleanup and retains its recovery intent", async () => {
    const fixture = resumeFixture();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      fixture.store,
    );
    fixture.dependencies.cleanupHandoff = async () => {
      throw new Error("synthetic cleanup failure");
    };
    await expect(
      resumePendingPortableExport(fixture.dependencies),
    ).rejects.toMatchObject({
      code: "cleanup-failed",
      committedResult: result,
    });
    expect(fixture.store.read()?.phase).toBe("published");
    expect(fixture.dependencies.onResult).not.toHaveBeenCalled();
  });

  it("preserves partial destination warnings when the Android copy rejects", async () => {
    const fixture = resumeFixture();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      fixture.store,
    );
    fixture.publishAndroid.mockRejectedValueOnce(
      new AndroidSafDestinationError(
        "11111111-1111-4111-8111-111111111111",
        "destination-copy-failed",
        ["partial-destination-may-remain"],
        "Synthetic destination failure",
      ),
    );
    await expect(
      resumePendingPortableExport(fixture.dependencies),
    ).rejects.toMatchObject({
      name: "PortableExportNeedsAttention",
      code: "destination-copy-failed",
      warningCodes: ["partial-destination-may-remain"],
      jobId: "synthetic-job",
    });
    expect(fixture.dependencies.cleanupHandoff).not.toHaveBeenCalled();
    expect(fixture.store.read()?.phase).toBe("publishing");
  });

  it("retains publication warnings across a renderer restart before cleanup", async () => {
    const fixture = resumeFixture();
    rememberPortableExport(
      "synthetic-job",
      { type: "androidSaf", suggestedName: "synthetic.risunest" },
      fixture.store,
    );
    fixture.store.write({
      ...fixture.store.read()!,
      phase: "published",
      requestId: "11111111-1111-4111-8111-111111111111",
      publicationWarningCodes: ["synthetic-publication-warning"],
    });
    await expect(
      resumePendingPortableExport(fixture.dependencies),
    ).resolves.toMatchObject({
      warningCodes: ["synthetic-publication-warning"],
    });
    expect(fixture.publishAndroid).not.toHaveBeenCalled();
    expect(fixture.resumeAndroid).not.toHaveBeenCalled();
  });
});
