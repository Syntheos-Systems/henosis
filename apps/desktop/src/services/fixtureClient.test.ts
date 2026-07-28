/** Contract tests proving browser fixtures cannot leak or imitate native tokens. */
import { describe, expect, it } from "vitest";
import { FixtureHenosisClient } from "./fixtureClient";

describe("FixtureHenosisClient", () => {
  it("labels its directory source and exposes no token-shaped fields", async () => {
    const client = new FixtureHenosisClient();

    const result = await client.bootstrap();
    const serialized = JSON.stringify(result);

    expect(result.directory?.source).toBe("fixture");
    expect(result.directory?.rooms.length).toBeGreaterThanOrEqual(3);
    expect(serialized).not.toMatch(/access.?token|refresh.?token/i);
  });
});
