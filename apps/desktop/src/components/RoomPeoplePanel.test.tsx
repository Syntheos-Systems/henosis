/** Presentation tests for truthful room-human and persistent-agent ownership grouping. */
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { AgentIdentity } from "../domain/agentControl";
import {
  createFixtureAgentIdentities,
  createFixtureRooms,
} from "../data/fixtureRooms";
import type { RoomSummary } from "../domain/rooms";
import { RoomPeoplePanel } from "./RoomPeoplePanel";

/** Return the primary room fixture required by People panel tests. */
function primaryRoom(): RoomSummary {
  const room = createFixtureRooms(new Date("2026-08-01T12:00:00.000Z"))[0];
  if (!room) {
    throw new Error("The People panel test requires the primary fixture room.");
  }
  return room;
}

describe("RoomPeoplePanel", () => {
  it("labels the stable current user as You without guessing a participant join", () => {
    render(
      <RoomPeoplePanel
        room={primaryRoom()}
        currentUserId="fixture-user"
        identities={createFixtureAgentIdentities()}
      />,
    );

    const currentHuman = screen.getByRole("group", { name: "You" });
    expect(within(currentHuman).getByText("Signed-in human")).toBeInTheDocument();
    expect(within(currentHuman).getByText("Mira")).toBeInTheDocument();
    expect(within(currentHuman).getByText("Lumen")).toBeInTheDocument();
    expect(within(currentHuman).queryByText("Operator")).not.toBeInTheDocument();
  });

  it("groups another human's agent and separates imported unowned identities", () => {
    render(
      <RoomPeoplePanel
        room={primaryRoom()}
        currentUserId="fixture-user"
        identities={createFixtureAgentIdentities()}
      />,
    );

    const rowan = screen.getByRole("group", { name: "Rowan" });
    expect(within(rowan).getByText("Cinder")).toBeInTheDocument();
    expect(within(rowan).getByText("Read-only member")).toBeInTheDocument();
    const unowned = screen.getByRole("group", { name: "Needs an owner" });
    expect(within(unowned).getByText("Imported scout")).toBeInTheDocument();
  });

  it("uses an opaque owner fallback when no room participant has that stable ID", () => {
    const identities: AgentIdentity[] = [
      ...createFixtureAgentIdentities(),
      {
        id: "agent-unknown-owner",
        username: "far-field",
        displayName: "Far field",
        ownerUserId: "human-not-in-summary",
      },
    ];
    render(
      <RoomPeoplePanel
        room={primaryRoom()}
        currentUserId="fixture-user"
        identities={identities}
      />,
    );

    const fallback = screen.getByRole("group", { name: "Another member" });
    expect(within(fallback).getByText("Far field")).toBeInTheDocument();
  });

  it("labels invitations as future work without rendering an invite mutation", () => {
    render(
      <RoomPeoplePanel
        room={primaryRoom()}
        currentUserId="fixture-user"
        identities={createFixtureAgentIdentities()}
      />,
    );

    expect(
      screen.getByText("Invitations are not part of this release."),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /invite/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: /email/i })).not.toBeInTheDocument();
  });
});
