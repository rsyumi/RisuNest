import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import assert from "node:assert/strict";

const directory = process.argv[2];
assert.ok(directory, "Expected synthetic result directory");
const read = (name) =>
  JSON.parse(readFileSync(join(directory, `${name}.json`), "utf8"));
const quantile = (values, p) =>
  values.toSorted((a, b) => a - b)[Math.ceil(values.length * p) - 1];
const stats = (values) => ({
  p50: quantile(values, 0.5),
  p95: quantile(values, 0.95),
  max: Math.max(...values),
});
const persistence = read("persistence");
const reload = read("reload");
const regex = read("regex");
assert.equal(reload.revision, persistence.revision);
assert.equal(reload.finalHash, persistence.finalHash);
const groups = [];
for (const size of [1048576, 6291456]) {
  for (const mode of ["json", "optimized"]) {
    const samples = persistence.samples.filter(
      (sample) => sample.size === size && sample.mode === mode,
    );
    assert.equal(samples.length, 20);
    groups.push({
      size,
      mode,
      n: samples.length,
      commands: [...new Set(samples.flatMap((sample) => sample.commands))],
      elapsedMs: stats(samples.map((sample) => sample.elapsedMs)),
      frameMs: stats(samples.flatMap((sample) => sample.frameGaps)),
    });
  }
}
assert.equal(regex.samples.length, 20);
assert.ok(persistence.memorySamples.some((sample) => sample.pssKiB > 0));
const summary = {
  persistence: groups,
  restartExact: true,
  memory: {
    peakPssKiB: Math.max(
      ...persistence.memorySamples.map((sample) => sample.pssKiB),
    ),
    peakUssKiB: Math.max(
      ...persistence.memorySamples.map((sample) => sample.ussKiB),
    ),
    scope: persistence.environment.memoryScope,
  },
  regex: {
    n: regex.samples.length,
    jsMs: stats(regex.samples.map((sample) => sample.jsMs)),
    nativeMs: stats(regex.samples.map((sample) => sample.nativeMs)),
  },
};
writeFileSync(
  join(directory, "summary.json"),
  JSON.stringify(summary, null, 2),
);
console.log(JSON.stringify(summary, null, 2));
