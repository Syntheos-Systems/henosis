/** Accessible tabs for the three room-control surfaces. */
import type { KeyboardEvent } from "react";

/** Stable identifiers for every room-dashboard tab. */
export type DashboardTabId = "agents" | "people" | "room";

/** Display metadata for one dashboard tab. */
interface DashboardTabDefinition {
  /** Stable identifier shared with its panel. */
  readonly id: DashboardTabId;
  /** Visible accessible label. */
  readonly label: string;
}

/** Ordered tabs used by both click and keyboard navigation. */
const DASHBOARD_TABS: readonly DashboardTabDefinition[] = [
  { id: "agents", label: "Agents" },
  { id: "people", label: "People" },
  { id: "room", label: "Room" },
];

/** Inputs for the controlled dashboard tablist. */
export interface DashboardTabsProps {
  /** Currently selected tab. */
  readonly activeTab: DashboardTabId;
  /** Commit one permitted tab selection. */
  readonly onSelect: (tab: DashboardTabId) => void;
  /** Optional navigation guard used when leaving a dirty surface. */
  readonly canSelect?: (tab: DashboardTabId) => boolean;
}

/** Return the DOM identifier for one tab. */
export function dashboardTabId(tab: DashboardTabId): string {
  return `dashboard-tab-${tab}`;
}

/** Return the DOM identifier for one tab panel. */
export function dashboardPanelId(tab: DashboardTabId): string {
  return `dashboard-panel-${tab}`;
}

/** Render automatic-selection WAI-ARIA tabs with wrapping arrow navigation. */
export function DashboardTabs({
  activeTab,
  onSelect,
  canSelect = () => true,
}: DashboardTabsProps) {
  /** Select and focus one tab when the navigation guard permits it. */
  function select(tab: DashboardTabId): boolean {
    if (!canSelect(tab)) {
      document.getElementById(dashboardTabId(activeTab))?.focus();
      return false;
    }
    onSelect(tab);
    document.getElementById(dashboardTabId(tab))?.focus();
    return true;
  }

  /** Apply WAI-ARIA arrow, Home, and End keyboard navigation. */
  function handleKeyDown(
    event: KeyboardEvent<HTMLButtonElement>,
    index: number,
  ): void {
    let nextIndex: number | undefined;
    if (event.key === "ArrowRight" || event.key === "ArrowDown") {
      nextIndex = (index + 1) % DASHBOARD_TABS.length;
    } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
      nextIndex = (index - 1 + DASHBOARD_TABS.length) % DASHBOARD_TABS.length;
    } else if (event.key === "Home") {
      nextIndex = 0;
    } else if (event.key === "End") {
      nextIndex = DASHBOARD_TABS.length - 1;
    }
    if (nextIndex === undefined) {
      return;
    }
    event.preventDefault();
    const next = DASHBOARD_TABS[nextIndex];
    if (next) {
      select(next.id);
    }
  }

  return (
    <div className="dashboard-tabs" role="tablist" aria-label="Room controls">
      {DASHBOARD_TABS.map((tab, index) => (
        <button
          className="dashboard-tab"
          id={dashboardTabId(tab.id)}
          key={tab.id}
          type="button"
          role="tab"
          aria-controls={dashboardPanelId(tab.id)}
          aria-selected={activeTab === tab.id}
          tabIndex={activeTab === tab.id ? 0 : -1}
          onClick={() => select(tab.id)}
          onKeyDown={(event) => handleKeyDown(event, index)}
        >
          {tab.label}
        </button>
      ))}
    </div>
  );
}
