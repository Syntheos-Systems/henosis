/** Unit tests for the public experience-profile fallback contract. */
import { describe, expect, it } from "vitest";
import {
  DEFAULT_DESKTOP_EXPERIENCE,
  normalizeDesktopExperience,
} from "./experience";

describe("normalizeDesktopExperience", () => {
  it("accepts every graphical profile", () => {
    expect(normalizeDesktopExperience("chat")).toBe("chat");
    expect(normalizeDesktopExperience("desktop")).toBe("desktop");
    expect(normalizeDesktopExperience("spatial")).toBe("spatial");
  });

  it("falls back safely for CLI and future persisted values", () => {
    expect(normalizeDesktopExperience("cli")).toBe(DEFAULT_DESKTOP_EXPERIENCE);
    expect(normalizeDesktopExperience("future-view")).toBe(
      DEFAULT_DESKTOP_EXPERIENCE,
    );
    expect(normalizeDesktopExperience(undefined)).toBe(
      DEFAULT_DESKTOP_EXPERIENCE,
    );
  });
});
