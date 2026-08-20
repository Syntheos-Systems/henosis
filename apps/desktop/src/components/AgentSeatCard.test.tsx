/** Interaction coverage for the room agent roster map and seat controls. */
import { useState } from "react";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type {
  AgentControlAction,
  AgentControlState,
} from "../domain/agentControl";
import {
  applyAgentControlAction,
  createAgentControlState,
} from "../domain/agentControl";
import {
  createFixtureAgentCatalog,
  createFixtureAgentIdentities,
  createFixtureAgentRosters,
} from "../data/fixtureRooms";
import { AgentRosterMap } from "./AgentRosterMap";

/** Build the primary manager-owned fixture control state. */
function createControlState(): AgentControlState {
  const snapshot = createFixtureAgentRosters().find(
    (candidate) => candidate.serverId === "server-henosis",
  );
  if (!snapshot) {
    throw new Error("The roster tests require the primary fixture snapshot.");
  }
  return createAgentControlState({
    currentHumanId: "fixture-user",
    canManageRoom: true,
    identities: createFixtureAgentIdentities(),
    catalog: createFixtureAgentCatalog(),
    snapshot,
  });
}

/** Exercise the production reducer behind the roster component. */
function StatefulRoster({ initialState = createControlState() }: { readonly initialState?: AgentControlState }) {
  const [state, setState] = useState(initialState);

  /** Apply one UI action through the immutable domain reducer. */
  function onAction(action: AgentControlAction): void {
    setState((current) => applyAgentControlAction(current, action));
  }

  return <AgentRosterMap control={state} onAction={onAction} />;
}

describe("AgentRosterMap", () => {
  it("renders topology, ownership, readiness, runtime, and separate capability controls", () => {
    render(<StatefulRoster />);

    expect(screen.getByRole("heading", { name: "Agent topology" })).toBeInTheDocument();
    expect(screen.getByText("3 agents in this room")).toBeInTheDocument();
    expect(screen.getByText("4 identities available")).toBeInTheDocument();
    expect(screen.getByText("Bridge active")).toBeInTheDocument();
    expect(screen.getByText("You own this identity")).toBeInTheDocument();
    expect(screen.getByText("Owned by another member")).toBeInTheDocument();
    expect(screen.getByText("Needs an owner")).toBeInTheDocument();
    expect(screen.getByText("Host session ready")).toBeInTheDocument();
    expect(screen.getByText("Credential ready")).toBeInTheDocument();
    expect(screen.getByText("Credential needs attention")).toBeInTheDocument();
    expect(screen.getAllByText("Active · revision 2")).toHaveLength(3);

    expect(screen.getByRole("combobox", { name: "Execution harness for Mira" })).toHaveValue(
      "codex-cli",
    );
    expect(screen.getByRole("combobox", { name: "Model for Mira" })).toHaveValue(
      "gpt-5.6-sol",
    );
    expect(screen.getByRole("combobox", { name: "Reasoning effort for Mira" })).toHaveValue(
      "medium",
    );
    expect(screen.getByRole("spinbutton", { name: "Turn limit for Mira" })).toHaveValue(8);
    expect(screen.getByRole("switch", { name: "Enable Mira in this room" })).toBeChecked();
  });

  it("filters models by harness and edits only catalog-defined settings", () => {
    render(<StatefulRoster />);

    fireEvent.change(screen.getByRole("combobox", { name: "Execution harness for Mira" }), {
      target: { value: "claude-code" },
    });
    const model = screen.getByRole("combobox", { name: "Model for Mira" });
    expect(model).toHaveValue("");
    expect(within(model).queryByRole("option", { name: "GPT-5.6 Sol" })).not.toBeInTheDocument();
    expect(within(model).getByRole("option", { name: "Claude Sonnet" })).toBeInTheDocument();
    expect(screen.queryByRole("spinbutton", { name: "Turn limit for Mira" })).not.toBeInTheDocument();

    fireEvent.change(model, { target: { value: "claude-sonnet" } });
    expect(model).toHaveValue("claude-sonnet");
  });

  it("disables foreign configuration while preserving manager-safe controls", () => {
    render(<StatefulRoster />);

    expect(screen.getByRole("combobox", { name: "Execution harness for Cinder" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Model for Cinder" })).toBeDisabled();
    const cinderEnabled = screen.getByRole("switch", { name: "Enable Cinder in this room" });
    expect(cinderEnabled).toBeEnabled();
    fireEvent.click(cinderEnabled);
    expect(cinderEnabled).not.toBeChecked();
    fireEvent.click(cinderEnabled);
    expect(cinderEnabled).toBeChecked();

    expect(
      screen.getByRole("switch", { name: "Enable Imported scout in this room" }),
    ).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove Cinder from room" })).toBeEnabled();
  });

  it("preserves authoritative unowned state when identity projections disagree", () => {
    const base = createControlState();
    const inconsistent: AgentControlState = {
      ...base,
      identities: base.identities.map((identity) =>
        identity.id === "agent-imported"
          ? { ...identity, ownerUserId: "fixture-user" }
          : identity,
      ),
    };

    render(<StatefulRoster initialState={inconsistent} />);

    expect(screen.getByText("Needs an owner")).toBeInTheDocument();
    expect(
      screen.getByRole("combobox", { name: "Execution harness for Imported scout" }),
    ).toBeDisabled();
  });

  it("disables dependent controls when the selected harness becomes unavailable", () => {
    const base = createControlState();
    const unavailable: AgentControlState = {
      ...base,
      catalog: {
        ...base.catalog,
        harnesses: base.catalog.harnesses.map((harness) =>
          harness.id === "codex-cli"
            ? {
                ...harness,
                available: false,
                unavailableReason: "Host discovery no longer finds Codex CLI.",
              }
            : harness,
        ),
      },
    };

    render(<StatefulRoster initialState={unavailable} />);

    expect(screen.getByRole("combobox", { name: "Execution harness for Mira" })).toBeEnabled();
    expect(screen.getByRole("combobox", { name: "Model for Mira" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Reasoning effort for Mira" })).toBeDisabled();
    expect(screen.getByRole("spinbutton", { name: "Turn limit for Mira" })).toBeDisabled();
    expect(screen.getByRole("switch", { name: "Web search for Mira" })).toBeDisabled();
  });

  it("adds and removes owned identities without deleting the persistent identity", () => {
    render(<StatefulRoster />);

    fireEvent.click(screen.getByRole("button", { name: "Add Lumen to room" }));
    expect(screen.getByRole("heading", { name: "Lumen" })).toBeInTheDocument();
    expect(screen.getByText("4 agents in this room")).toBeInTheDocument();
    expect(screen.getByText("Not yet applied")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Remove Lumen from room" }));
    expect(screen.queryByRole("heading", { name: "Lumen" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add Lumen to room" })).toBeInTheDocument();
  });

  it("shows failed activation and the last known good revision", () => {
    const base = createControlState();
    const failed: AgentControlState = {
      ...base,
      serverSnapshot: {
        ...base.serverSnapshot,
        desiredRevision: 3,
        activeRevision: 2,
        lastGoodRevision: 2,
        runtimeActivation: "failed",
        runtimeErrorCode: "harness_start_failed",
        runtimeErrorMessage: "The selected harness did not start.",
      },
      bridgeStatus: {
        ...base.bridgeStatus,
        desiredRevision: 3,
        activeRevision: 2,
        lastGoodRevision: 2,
        runtimeActivation: "failed",
        runtimeErrorCode: "harness_start_failed",
        runtimeErrorMessage: "The selected harness did not start.",
      },
    };

    render(<StatefulRoster initialState={failed} />);

    expect(screen.getByText("Bridge failed")).toBeInTheDocument();
    expect(screen.getAllByText("Failed · harness_start_failed")).toHaveLength(3);
    expect(screen.getAllByText("Last good revision 2")).toHaveLength(3);
  });
});
