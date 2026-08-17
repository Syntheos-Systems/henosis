/** Integrated room conversation workspace reached from the room selector. */
import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { ArrowLeft, BellDot, PanelRight, X } from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import type { HenosisClient } from "../services/henosisClient";
import { RoomConversation } from "./RoomConversation";
import { RoomDashboard } from "./RoomDashboard";

/** Viewport query separating the persistent aside from the modal sheet. */
const DASHBOARD_ASIDE_QUERY = "(min-width: 1180px)";

/** Selectors for controls allowed in the narrow dashboard focus loop. */
const DASHBOARD_FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** User-decision callback used by the dirty-navigation boundary. */
export type ConfirmDashboardDiscard = (message: string) => boolean;

/** Ask before abandoning one dirty room-control draft. */
export function confirmDashboardNavigation(
  dirty: boolean,
  confirmDiscard: ConfirmDashboardDiscard = (message) => window.confirm(message),
): boolean {
  return (
    !dirty ||
    confirmDiscard(
      "Discard unsaved room control changes and leave this room? Choose Cancel to remain.",
    )
  );
}

/** Read the current dashboard presentation without guessing during SSR. */
function dashboardUsesAside(): boolean {
  if (typeof window === "undefined") {
    return true;
  }
  return typeof window.matchMedia === "function"
    ? window.matchMedia(DASHBOARD_ASIDE_QUERY).matches
    : true;
}

/** Track the semantic dashboard presentation at the product breakpoint. */
function useDashboardAside(): boolean {
  const [usesAside, setUsesAside] = useState(dashboardUsesAside);

  useEffect(() => {
    if (typeof window.matchMedia !== "function") {
      return undefined;
    }
    const media = window.matchMedia(DASHBOARD_ASIDE_QUERY);
    /** Project one media-query change into component state. */
    function updatePresentation(): void {
      setUsesAside(media.matches);
    }
    updatePresentation();
    media.addEventListener("change", updatePresentation);
    return () => media.removeEventListener("change", updatePresentation);
  }, []);

  return usesAside;
}

/** Return the focusable controls participating in the modal sheet loop. */
function dashboardFocusableElements(container: HTMLElement): HTMLElement[] {
  return Array.from(
    container.querySelectorAll<HTMLElement>(DASHBOARD_FOCUSABLE_SELECTOR),
  ).filter(
    (element) =>
      !element.hidden &&
      element.tabIndex >= 0 &&
      element.getAttribute("aria-hidden") !== "true",
  );
}

/** Inputs for entering one room from the room selector. */
export interface RoomDetailProps {
  /** Shared native or fixture adapter owned by the application shell. */
  client: HenosisClient;
  /** Selected room summary. */
  room: RoomSummary;
  /** Explicit authenticated human, absent for a disconnected cache. */
  currentUserId: string | undefined;
  /** Return to the room selector without leaving Henosis. */
  onBack(): void;
  /** Return to connection setup when live room controls cannot load. */
  onReconnect(): void;
  /** Explain a room control unavailable in the current build. */
  onUnavailableAction(action: string): void;
}

/** Render a visible room workspace instead of hiding Rift behind a terminal. */
export function RoomDetail({
  client,
  room,
  currentUserId,
  onBack,
  onReconnect,
  onUnavailableAction,
}: RoomDetailProps) {
  const usesAside = useDashboardAside();
  const [sheetOpen, setSheetOpen] = useState(false);
  const [dashboardDirty, setDashboardDirty] = useState(false);
  const dashboardTriggerRef = useRef<HTMLButtonElement>(null);
  const dashboardSurfaceRef = useRef<HTMLElement>(null);
  const dashboardCloseRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (usesAside && sheetOpen) {
      setSheetOpen(false);
      window.setTimeout(() => {
        dashboardSurfaceRef.current
          ?.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]')
          ?.focus();
      }, 0);
    }
  }, [sheetOpen, usesAside]);

  useEffect(() => {
    if (!usesAside && sheetOpen) {
      dashboardCloseRef.current?.focus();
    }
  }, [sheetOpen, usesAside]);

  /** Open the narrow room-control sheet without changing conversation state. */
  function openDashboardSheet(): void {
    setSheetOpen(true);
  }

  /** Close the narrow sheet and restore its invoking control. */
  function closeDashboardSheet(): void {
    setSheetOpen(false);
    window.setTimeout(() => dashboardTriggerRef.current?.focus(), 0);
  }

  /** Keep Tab and Shift+Tab inside the open narrow dashboard sheet. */
  function handleDashboardKeyDown(event: KeyboardEvent<HTMLElement>): void {
    if (usesAside || !sheetOpen) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeDashboardSheet();
      return;
    }
    if (event.key !== "Tab" || !dashboardSurfaceRef.current) {
      return;
    }
    const focusable = dashboardFocusableElements(dashboardSurfaceRef.current);
    const first = focusable[0];
    const last = focusable.at(-1);
    if (!first || !last) {
      event.preventDefault();
      return;
    }
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  /** Leave the room only after an explicit dirty-draft decision. */
  function handleBack(): void {
    if (confirmDashboardNavigation(dashboardDirty)) {
      onBack();
    }
  }

  return (
    <main className="room-detail" id="main-content">
      <header className="room-detail-header">
        <button className="back-button" type="button" onClick={handleBack}>
          <ArrowLeft aria-hidden="true" />
          All rooms
        </button>
        <div className="room-detail-title">
          <span className="room-detail-glyph" aria-hidden="true">
            #
          </span>
          <div>
            <p>{room.serverName}</p>
            <h1>{room.name}</h1>
          </div>
        </div>
        <div className="room-detail-actions">
          {room.pendingApprovals > 0 ? (
            <button
              className="button button-secondary room-approvals-button"
              type="button"
              onClick={() => onUnavailableAction("Open room approvals")}
            >
              <BellDot aria-hidden="true" />
              {room.pendingApprovals} waiting
            </button>
          ) : null}
          <button
            ref={dashboardTriggerRef}
            className="room-dashboard-trigger"
            type="button"
            aria-label="Open room dashboard"
            aria-controls="room-dashboard-surface"
            aria-expanded={!usesAside && sheetOpen}
            onClick={openDashboardSheet}
          >
            <PanelRight aria-hidden="true" />
            <span>Room controls</span>
          </button>
        </div>
      </header>

      <div className="room-detail-grid">
        <RoomConversation client={client} roomId={room.id} />

        {!usesAside && sheetOpen ? (
          <div
            className="room-dashboard-backdrop"
            aria-hidden="true"
            onClick={closeDashboardSheet}
          />
        ) : null}

        <aside
          ref={dashboardSurfaceRef}
          className="room-dashboard-surface"
          id="room-dashboard-surface"
          aria-label="Room dashboard"
          aria-modal={usesAside ? undefined : true}
          hidden={!usesAside && !sheetOpen}
          role={usesAside ? undefined : "dialog"}
          onKeyDown={handleDashboardKeyDown}
        >
          {!usesAside ? (
            <button
              ref={dashboardCloseRef}
              className="room-dashboard-sheet-close"
              type="button"
              aria-label="Close room controls"
              onClick={closeDashboardSheet}
            >
              <X aria-hidden="true" />
            </button>
          ) : null}
          <RoomDashboard
            client={client}
            room={room}
            currentUserId={currentUserId}
            onReconnect={onReconnect}
            onDirtyChange={setDashboardDirty}
          />
        </aside>
      </div>
    </main>
  );
}
