type Mode = "recent" | "collapsed" | "off";
interface FixtureApi {
  configure(mode: Mode, defer: boolean): Promise<void>;
  publish(source: string, active?: boolean): Promise<void>;
  sourceMatches(source: string): boolean;
  setRemoval(enabled: boolean): void;
}

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
const root = () => document.querySelector("#streaming-body")!;
const preview = () =>
  root().querySelector<HTMLElement>("[data-streaming-thought-preview]");
function progress(phase: string) {
  (
    window as typeof window & {
      __streamingSmokeProgress?: (phase: string) => void;
    }
  ).__streamingSmokeProgress?.(phase);
}
function check(ok: unknown, id: string): asserts ok {
  if (!ok) throw new Error(id);
}
async function until(predicate: () => boolean, id: string) {
  const deadline = performance.now() + 15000;
  while (!predicate()) {
    check(performance.now() < deadline, id);
    await pause(16);
  }
}
const percentile = (values: number[], ratio: number) => {
  if (!values.length) return null;
  return [...values].sort((a, b) => a - b)[
    Math.ceil(values.length * ratio) - 1
  ];
};

export async function runStreamingSuite(api: FixtureApi, profile = "smoke") {
  const cases: Record<string, unknown>[] = [];
  let phase = "start";
  try {
    for (const mode of ["recent", "collapsed", "off"] as const) {
      for (const defer of [true, false]) {
        phase = `${mode}-${defer ? "deferred" : "effects"}`;
        progress(phase);
        api.setRemoval(false);
        await api.configure(mode, defer);
        await until(
          () => !!root().textContent?.includes("Synthetic initial answer"),
          "initial",
        );
        // Keep the general behavior smoke separate from the retained long-text
        // stress reproduction. Some older Android WebViews stall during layout.
        const thought = "합성 추론 🐿️ ".repeat(
          profile === "stress" ? 50000 : 1000,
        );
        const source =
          "<Thoughts>" + thought + "\nLATEST-SYNTHETIC</Thoughts>\n**Answer**";
        const started = performance.now();
        await api.publish(source);
        await until(
          () =>
            mode === "collapsed"
              ? !!root().querySelector("details")
              : !!root().textContent?.includes("LATEST-SYNTHETIC"),
          "first-visible",
        );
        const firstVisibleMs = performance.now() - started;
        check(api.sourceMatches(source), "source-preserved");
        if (mode === "recent") {
          const text = root().querySelector<HTMLElement>(
            ".x-risu-streaming-thought-text",
          )!;
          const window = root().querySelector<HTMLElement>(
            ".x-risu-streaming-thought-window",
          )!;
          check(text?.textContent.length <= 800, "bounded-text");
          check(
            window.getBoundingClientRect().height <=
              parseFloat(getComputedStyle(window).lineHeight) * 4 + 1,
            "bounded-height",
          );
          check(!preview()?.querySelector("strong, a, img"), "literal-thought");
        }
        if (mode === "collapsed") {
          const details = root().querySelector("details")!;
          check(!details.open, "initially-collapsed");
          if (defer) check(details.textContent.length < 100, "lazy-body");
          progress(phase + "-expand");
          details.querySelector("summary")!.click();
          await until(
            () =>
              details.open &&
              !!details.textContent?.includes("LATEST-SYNTHETIC"),
            "expanded-body",
          );
          await api.publish(
            source.replace("LATEST-SYNTHETIC", "UPDATED-SYNTHETIC"),
          );
          await until(
            () =>
              !!root()
                .querySelector("details")
                ?.textContent?.includes("UPDATED-SYNTHETIC"),
            "expanded-update",
          );
          check(
            root().querySelector("details")?.open,
            "expanded-state-retained",
          );
        }
        // Completion must execute the real editdisplay regex against the whole input.
        api.setRemoval(true);
        const finalStarted = performance.now();
        progress(phase + "-final");
        await api.publish(source, false);
        await until(
          () =>
            root().querySelector("strong")?.textContent === "Answer" &&
            !root().querySelector("details") &&
            !preview(),
          "canonical-final",
        );
        check(
          !root().textContent?.includes("LATEST-SYNTHETIC"),
          "final-removal",
        );
        check(api.sourceMatches(source), "final-source-preserved");
        cases.push({
          id: phase,
          passed: true,
          sourceLength: source.length,
          firstVisibleMs,
          finalVisibleMs: performance.now() - finalStarted,
        });
      }
    }

    phase = "split-nested-replacement";
    progress(phase);
    api.setRemoval(false);
    await api.configure("recent", true);
    for (const source of [
      "<Thou",
      "<Thoughts>OUTER<Thou",
      "<Thoughts>OUTER<Thoughts>INNER</Thoughts>TAIL",
      "<Thoughts>OUTER<Thoughts>INNER</Thoughts>TAIL</Thoughts>ANSWER",
      "<Thoughts>OUTER<Thoughts>INNER</Thoughts>INSERTED</Thoughts>ANSWER",
    ]) {
      await api.publish(source);
      await pause(35);
      check(api.sourceMatches(source), "split-source");
    }
    check(preview()?.textContent?.includes("INSERTED"), "replacement-visible");
    check(
      !preview()?.textContent?.includes("<Thoughts>"),
      "nested-delimiters-hidden",
    );
    await api.publish("<Thoughts>UNFINISHED", false);
    await until(
      () => !preview() && !!root().textContent?.includes("UNFINISHED"),
      "cancel-final",
    );
    cases.push({ id: phase, passed: true });

    for (const hz of [30, 1000]) {
      phase = `cadence-${hz}`;
      progress(phase);
      await api.configure("recent", true);
      await pause(100);
      const gaps: number[] = [];
      const longTasks: number[] = [];
      const supported =
        PerformanceObserver.supportedEntryTypes.includes("longtask");
      const observer = supported
        ? new PerformanceObserver((entries) => {
            longTasks.push(
              ...entries.getEntries().map((entry) => entry.duration),
            );
          })
        : null;
      observer?.observe({ type: "longtask" });
      let previous = performance.now();
      let frame = 0;
      const sample = (now: number) => {
        gaps.push(now - previous);
        previous = now;
        frame = requestAnimationFrame(sample);
      };
      frame = requestAnimationFrame(sample);
      const prefix = "<Thoughts>" + "합성 추론 🐿️ ".repeat(50000);
      const started = performance.now();
      for (let i = 0; i < 60; i++) {
        await api.publish(prefix + `\nFRAME-${i}</Thoughts>Answer ${i}`);
        await pause(1000 / hz);
      }
      await until(
        () => !!preview()?.textContent?.includes("FRAME-59"),
        "latest-frame",
      );
      await pause(50);
      cancelAnimationFrame(frame);
      observer?.disconnect();
      cases.push({
        id: phase,
        passed: true,
        updates: 60,
        sourceLength: prefix.length,
        elapsedMs: performance.now() - started,
        frameSamples: gaps.length,
        frameGapP95Ms: percentile(gaps, 0.95),
        frameGapMaxMs: gaps.length ? Math.max(...gaps) : null,
        longTasks: supported ? longTasks.length : null,
        longTaskMaxMs: supported ? Math.max(0, ...longTasks) : null,
      });
    }
    return { passed: true, cases };
  } catch (error) {
    return {
      passed: false,
      phase,
      assertion: error instanceof Error ? error.message : "unknown",
      cases,
    };
  }
}
