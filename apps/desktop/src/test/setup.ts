/** Shared DOM matchers and cleanup behavior for Henosis component tests. */
import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

/** Remove rendered trees so each test begins with an isolated document. */
afterEach(() => cleanup());
