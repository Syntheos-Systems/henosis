/** Interaction tests for read-only room context and manager bridge controls. */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { RoomBridgeStatus } from "../domain/agentControl";
import { createFixtureRooms } from "../data/fixtureRooms";
import type { RoomSummary } from "../domain/rooms";
import { HenosisClientError } from "../services/henosisClient";
import { RoomSettingsPanel } from "./RoomSettingsPanel";

/** Return the primary room fixture required by Room panel tests. */
function primaryRoom(): RoomSummary {
  const room = createFixtureRooms(new Date("2026-08-01T12:00:00.000Z"))[0];
  if (!room) {
    throw new Error("The Room panel test requires the primary fixture room.");
  }
  return room;
}

/** Build one active bridge status with narrow overrides. */
function bridgeStatus(overrides: Partial<RoomBridgeStatus> = {}): RoomBridgeStatus {
  return {
    paused: false,
    desiredRevision: 7,
    activeRevision: 6,
    lastGoodRevision: 6,
    runtimeActivation: "pending",
    runtimeErrorCode: null,
    runtimeErrorMessage: null,
    ...overrides,
  };
}

describe("RoomSettingsPanel", () => {
  it("renders read-only metadata, approvals, connection state, capability, and revisions", () => {
    const room: RoomSummary = { ...primaryRoom(), pendingApprovals: 2 };
    render(
      <RoomSettingsPanel
        room={room}
        status={bridgeStatus()}
        canManageRoom={true}
        onPause={vi.fn()}
        onResume={vi.fn()}
      />,
    );

    expect(screen.getByRole("heading", { name: "orchard" })).toBeInTheDocument();
    expect(screen.getByText("Runtime integration and release work")).toBeInTheDocument();
    expect(screen.getByText("Henosis")).toBeInTheDocument();
    expect(screen.getByText("4 visible members")).toBeInTheDocument();
    expect(screen.getByText("2 pending approvals")).toBeInTheDocument();
    expect(screen.getByText("Active room")).toBeInTheDocument();
    expect(screen.getByText("Room manager")).toBeInTheDocument();
    expect(screen.getByText("Desired revision 7")).toBeInTheDocument();
    expect(screen.getByText("Active revision 6")).toBeInTheDocument();
    expect(screen.getByText("Last good revision 6")).toBeInTheDocument();
  });

  it("lets a manager pause or resume through distinct async callbacks", async () => {
    const onPause = vi.fn(async () => undefined);
    const onResume = vi.fn(async () => undefined);
    const { rerender } = render(
      <RoomSettingsPanel
        room={primaryRoom()}
        status={bridgeStatus()}
        canManageRoom={true}
        onPause={onPause}
        onResume={onResume}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Pause bridge" }));
    await waitFor(() => expect(onPause).toHaveBeenCalledOnce());

    rerender(
      <RoomSettingsPanel
        room={primaryRoom()}
        status={bridgeStatus({ paused: true })}
        canManageRoom={true}
        onPause={onPause}
        onResume={onResume}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Resume bridge" }));
    await waitFor(() => expect(onResume).toHaveBeenCalledOnce());
  });

  it("renders member status without any bridge mutation control", () => {
    render(
      <RoomSettingsPanel
        room={primaryRoom()}
        status={bridgeStatus()}
        canManageRoom={false}
        onPause={vi.fn()}
        onResume={vi.fn()}
      />,
    );

    expect(screen.getByText("Member access")).toBeInTheDocument();
    expect(
      screen.getByText("Bridge controls require room-manager access."),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /bridge/i })).not.toBeInTheDocument();
  });

  it("keeps current status visible and reports a normalized bridge failure", async () => {
    const onPause = vi.fn(async () => {
      throw new HenosisClientError("network", "The bridge control request did not reach Rift.");
    });
    render(
      <RoomSettingsPanel
        room={primaryRoom()}
        status={bridgeStatus()}
        canManageRoom={true}
        onPause={onPause}
        onResume={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Pause bridge" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "The bridge control request did not reach Rift.",
    );
    expect(screen.getByText("Bridge running")).toBeInTheDocument();
  });

  it("does not expose room rename, invitations, or direct-message settings", () => {
    render(
      <RoomSettingsPanel
        room={primaryRoom()}
        status={bridgeStatus()}
        canManageRoom={true}
        onPause={vi.fn()}
        onResume={vi.fn()}
      />,
    );

    expect(
      screen.queryByRole("textbox", { name: /room name/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /invite|rename|direct message/i }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText(/direct messages/i)).not.toBeInTheDocument();
  });
});
