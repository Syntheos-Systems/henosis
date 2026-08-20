/** Compact renderer selector shared by every graphical Henosis profile. */
import { LayoutDashboard, MessageSquareText, Orbit } from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { DESKTOP_EXPERIENCES } from "../domain/experience";
/** Graphical profile identifiers accepted by this control. */
import type { DesktopExperienceProfile } from "../domain/experience";

/** Icon associated with each graphical experience profile. */
const EXPERIENCE_ICONS: Record<DesktopExperienceProfile, LucideIcon> = {
  chat: MessageSquareText,
  desktop: LayoutDashboard,
  spatial: Orbit,
};

/** Inputs for the graphical experience selector. */
export interface ExperienceSelectorProps {
  /** Currently active graphical profile. */
  value: DesktopExperienceProfile;
  /** True while native preference persistence is in flight. */
  busy: boolean;
  /** Persist and activate one graphical profile. */
  onChange(profile: DesktopExperienceProfile): void;
}

/** Render a keyboard-accessible single-choice profile switcher. */
export function ExperienceSelector({
  value,
  busy,
  onChange,
}: ExperienceSelectorProps) {
  return (
    <div className="experience-selector" aria-label="Henosis experience">
      {DESKTOP_EXPERIENCES.map((option) => {
        const Icon = EXPERIENCE_ICONS[option.id];
        return (
          <button
            className="experience-option"
            type="button"
            data-active={option.id === value}
            aria-pressed={option.id === value}
            aria-label={`${option.label}: ${option.description}`}
            disabled={busy}
            onClick={() => onChange(option.id)}
            key={option.id}
          >
            <Icon aria-hidden="true" />
            <span>{option.label}</span>
          </button>
        );
      })}
    </div>
  );
}
