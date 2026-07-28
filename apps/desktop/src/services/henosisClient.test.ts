/** Error-boundary tests that ensure Henosis keeps native details out of the GUI. */
import { describe, expect, it } from "vitest";
import { normalizeClientError } from "./henosisClient";

describe("normalizeClientError", () => {
  it("preserves structured native recovery guidance", () => {
    const result = normalizeClientError({
      kind: "network",
      message: "Check the Rift endpoint.",
    });

    expect(result.kind).toBe("network");
    expect(result.message).toBe("Check the Rift endpoint.");
  });

  it("parses structured Tauri errors serialized as JSON", () => {
    const result = normalizeClientError(
      JSON.stringify({
        kind: "authentication",
        message: "Sign in again.",
      }),
    );

    expect(result.kind).toBe("authentication");
    expect(result.message).toBe("Sign in again.");
  });

  it("redacts unstructured rejection strings", () => {
    const result = normalizeClientError(
      "transport failed with password=do-not-render",
    );

    expect(result.kind).toBe("unknown");
    expect(result.message).not.toContain("do-not-render");
    expect(result.message).toContain("reconnect to Rift");
  });
});
