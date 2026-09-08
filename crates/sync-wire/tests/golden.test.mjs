import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
import test from "node:test";

// Independent ECMAScript reference for the restricted control profile. JS sort
// compares UTF-16 code units, and JSON.stringify supplies JCS string escaping.
function canonical(value) {
  if (typeof value === "number") throw new Error("numbers-forbidden");
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

test("Rust and ECMAScript agree on restricted JCS golden bytes", () => {
  const vectors = JSON.parse(
    readFileSync(new URL("./golden.json", import.meta.url), "utf8"),
  );
  for (const vector of vectors)
    assert.equal(canonical(vector.input), vector.canonical);
  assert.throws(() => canonical({ size: 9007199254740993 }));
});
