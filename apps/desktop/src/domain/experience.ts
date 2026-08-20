/** Public experience profiles supported by the Henosis installer and desktop. */
export type ExperienceProfile = "cli" | "chat" | "desktop" | "spatial";

/** Graphical profiles available inside the native desktop application. */
export type DesktopExperienceProfile = Exclude<ExperienceProfile, "cli">;

/** Human-facing metadata for one graphical Henosis experience. */
export interface DesktopExperienceOption {
  /** Stable serialized profile value. */
  id: DesktopExperienceProfile;
  /** Compact selector label. */
  label: string;
  /** Concise explanation of the profile's primary focus. */
  description: string;
}

/** Safe graphical fallback used for absent or future preference values. */
export const DEFAULT_DESKTOP_EXPERIENCE: DesktopExperienceProfile = "desktop";

/** Ordered graphical choices shown by every desktop renderer. */
export const DESKTOP_EXPERIENCES: readonly DesktopExperienceOption[] = [
  {
    id: "chat",
    label: "Chat",
    description: "Rooms and conversation stay in focus.",
  },
  {
    id: "desktop",
    label: "Desktop",
    description: "Complete room, agent, and runtime controls.",
  },
  {
    id: "spatial",
    label: "Spatial",
    description: "Navigate live rooms and agents as places.",
  },
];

/** Normalize untrusted persisted input into a supported graphical profile. */
export function normalizeDesktopExperience(
  value: unknown,
): DesktopExperienceProfile {
  return value === "chat" || value === "spatial" || value === "desktop"
    ? value
    : DEFAULT_DESKTOP_EXPERIENCE;
}
