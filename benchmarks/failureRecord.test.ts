import { describe, expect, it } from "vitest";
import { LuaWorkerExecutor } from "../src/ts/process/luaWorkerExecutor";
import { failureRecord } from "./failureRecord";

describe("failureRecord", () => {
  it("records the cause chain of a Lua Worker that could not start", () => {
    const blocked = new TypeError("Worker script was blocked", { cause: new RangeError("synthetic quota") });
    let error: unknown;
    try {
      new LuaWorkerExecutor(5_000, undefined, () => { throw blocked; });
    } catch (thrown) {
      error = thrown;
    }
    const record = failureRecord(error);
    expect(record).toMatchObject({
      name: "Error",
      message: "Lua Worker could not start",
      stack: expect.any(String),
      causes: [
        { name: "TypeError", message: "Worker script was blocked", stack: expect.any(String) },
        { name: "RangeError", message: "synthetic quota", stack: expect.any(String) },
      ],
    });
    expect(record.causesCut).toBeUndefined();
    // The record reaches the log as JSON, which drops the fields of an Error itself.
    expect(JSON.parse(JSON.stringify(record)).causes[1].message).toBe("synthetic quota");
  });

  it("stops at a repeated cause or the limit and keeps causes that are not errors", () => {
    const first = new Error("first");
    const second = new Error("second", { cause: first });
    first.cause = second;
    expect(failureRecord(first)).toMatchObject({ message: "first", causes: [{ message: "second" }], causesCut: "cycle" });

    let chain: unknown = { code: 7 };
    for (let index = 0; index < 4; index++) chain = new Error(`level ${index}`, { cause: chain });
    expect(failureRecord(chain, 2)).toMatchObject({
      message: "level 3",
      causes: [{ message: "level 2" }, { message: "level 1" }],
      causesCut: "limit",
    });
    expect(failureRecord(chain).causes!.map(cause => cause.message)).toEqual(["level 2", "level 1", "level 0", '{"code":7}']);
  });
});
