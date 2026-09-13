import { describe, expect, it } from "vitest";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
import {
  encodeServerRegistration,
  parseServerRegistration,
  REGISTRATION_PREFIX,
} from "./serverSyncRegistration";
const wrap = (json: string) =>
  REGISTRATION_PREFIX + Buffer.from(json).toString("base64url");
describe("server registration", () => {
  it("matches the daemon/native golden vector and preserves all credentials", () => {
    expect(encodeServerRegistration(vector.registration)).toBe(vector.uri);
    expect(parseServerRegistration(vector.uri)).toEqual(vector.registration);
    const fixed = { ...vector.registration, directory: undefined };
    expect(parseServerRegistration(encodeServerRegistration(fixed))).toEqual(
      fixed,
    );
  });
  it("rejects duplicate, unknown, partial and null fields", () => {
    const valid = JSON.stringify(vector.registration);
    for (const value of [
      valid.replace("{", '{"endpoint":"https://evil.example",'),
      valid.replace("{", '{"extra":"secret",'),
      valid.replace('"directory":{', '"directory":{"key":"secret",'),
      valid.replace(/"directory":.*}/, '"directory":null}'),
      valid.replace('"key":', '"unknown":'),
    ]) {
      expect(() => parseServerRegistration(wrap(value))).toThrow();
    }
    expect(() =>
      parseServerRegistration(
        wrap(
          valid.replace(
            '"endpoint":',
            '"end\\u0070oint":"https://evil.example","endpoint":',
          ),
        ),
      ),
    ).toThrow();
  });
  it("rejects malformed and noncanonical transport without echoing secrets", () => {
    for (const value of [
      vector.uri + "=",
      vector.uri + "&x=secret",
      REGISTRATION_PREFIX + "_w",
      "https://sync.example/#secret",
      REGISTRATION_PREFIX + "x".repeat(2049),
    ]) {
      expect(() => parseServerRegistration(value)).toThrow();
    }
    try {
      parseServerRegistration("secret");
    } catch (error) {
      expect(String(error)).not.toContain("secret");
    }
  });
  it("enforces safe endpoint, identity and directory contracts", () => {
    for (const endpoint of [
      "https://@sync.example",
      "https://a:b@sync.example",
      "http://example.com",
      "https://sync.example?",
      "https://sync.example#",
      "https://sync.example/ white",
    ]) {
      expect(() =>
        encodeServerRegistration({ ...vector.registration, endpoint }),
      ).toThrow();
    }
    for (const key of ["a".repeat(42), "A".repeat(42) + "B"])
      expect(() =>
        encodeServerRegistration({
          ...vector.registration,
          directory: { ...vector.registration.directory, key },
        }),
      ).toThrow();
  });
});
