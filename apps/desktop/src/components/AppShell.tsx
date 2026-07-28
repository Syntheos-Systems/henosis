/** Persistent Henosis navigation and global connection context. */
import type { ReactNode } from "react";
import {
  Activity,
  Bot,
  BrainCircuit,
  ChevronDown,
  CircleGauge,
  DoorOpen,
  FileLock2,
  GalleryVerticalEnd,
  MessageSquareText,
  Settings,
  Sparkles,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import type {
  RoomDirectorySnapshot,
  SanitizedConnection,
} from "../services/henosisClient";

/** One visible Henosis workspace navigation item. */
interface WorkspaceItem {
  /** Stable workspace identifier. */
  id: string;
  /** Human-facing workspace name. */
  label: string;
  /** Workspace icon. */
  icon: LucideIcon;
  /** Whether this delivery slice exposes the workspace. */
  available: boolean;
}

/** Product workspaces arranged around operator intent. */
const WORKSPACES: WorkspaceItem[] = [
  { id: "rooms", label: "Rooms", icon: MessageSquareText, available: true },
  { id: "athena", label: "Athena", icon: Sparkles, available: false },
  { id: "agents", label: "Agents", icon: Bot, available: false },
  { id: "work", label: "Work", icon: GalleryVerticalEnd, available: false },
  { id: "memory", label: "Memory", icon: BrainCircuit, available: false },
  { id: "governance", label: "Governance", icon: FileLock2, available: false },
  { id: "activity", label: "Activity", icon: Activity, available: false },
];

/** Inputs for the application-wide Henosis shell. */
export interface AppShellProps {
  /** Current room data used for connection and approval indicators. */
  directory?: RoomDirectorySnapshot;
  /** Authenticated display identity when available. */
  connection?: SanitizedConnection;
  /** Primary workspace view. */
  children: ReactNode;
  /** Return to Rooms from an internal workspace. */
  onRooms(): void;
  /** Explain an unfinished workspace without pretending it exists. */
  onDeferredWorkspace(workspace: string): void;
}

/** Render Henosis branding, workspace navigation, and global system state. */
export function AppShell({
  directory,
  connection,
  children,
  onRooms,
  onDeferredWorkspace,
}: AppShellProps) {
  const approvalCount =
    directory?.rooms.reduce((sum, room) => sum + room.pendingApprovals, 0) ?? 0;
  const sourceLabel =
    directory?.source === "fixture"
      ? "Preview data"
      : directory?.connected
        ? "Rift connected"
        : directory
          ? "Cached rooms"
          : "Connect Rift";

  return (
    <div className="app-shell">
      <a className="skip-link" href="#main-content">
        Skip to rooms
      </a>

      <aside className="app-sidebar" aria-label="Henosis navigation">
        <div className="brand-lockup">
          <span className="brand-mark" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
          <span>
            <strong>Henosis</strong>
            <small>Agent environment</small>
          </span>
        </div>

        <nav className="workspace-nav" aria-label="Workspaces">
          {WORKSPACES.map((workspace) => {
            const Icon = workspace.icon;
            return (
              <button
                className="workspace-link"
                data-active={workspace.id === "rooms"}
                type="button"
                aria-current={workspace.id === "rooms" ? "page" : undefined}
                onClick={() =>
                  workspace.available
                    ? onRooms()
                    : onDeferredWorkspace(workspace.label)
                }
                key={workspace.id}
              >
                <Icon aria-hidden="true" />
                <span>{workspace.label}</span>
                {!workspace.available ? <small>Later</small> : null}
              </button>
            );
          })}
        </nav>

        <div className="sidebar-lower">
          <button
            className="workspace-link"
            type="button"
            onClick={() => onDeferredWorkspace("Settings")}
          >
            <Settings aria-hidden="true" />
            <span>Settings</span>
            <small>Later</small>
          </button>

          <div className="system-card">
            <div className="system-card-title">
              <CircleGauge aria-hidden="true" />
              <span>System pulse</span>
            </div>
            <div className="system-metric">
              <strong>{directory?.rooms.length ?? 0}</strong>
              <span>rooms visible</span>
            </div>
            <div className="system-rule" />
            <div className="system-status">
              <span data-connected={directory?.connected} />
              {sourceLabel}
            </div>
          </div>
        </div>
      </aside>

      <div className="app-stage">
        <header className="global-bar">
          <button className="context-switcher" type="button" onClick={onRooms}>
            <DoorOpen aria-hidden="true" />
            <span>
              <small>Workspace</small>
              Rooms
            </span>
            <ChevronDown aria-hidden="true" />
          </button>

          <div className="global-actions">
            {approvalCount > 0 ? (
              <button
                className="approval-indicator"
                type="button"
                onClick={() => onDeferredWorkspace("Governance")}
              >
                {approvalCount} approval{approvalCount === 1 ? "" : "s"}
              </button>
            ) : null}
            <div className="global-identity">
              <span>{connection?.displayName.slice(0, 2).toLocaleUpperCase() ?? "H"}</span>
              <div>
                <strong>{connection?.displayName ?? "Henosis operator"}</strong>
                <small>{sourceLabel}</small>
              </div>
            </div>
          </div>
        </header>

        <div className="app-content">{children}</div>
      </div>
    </div>
  );
}
