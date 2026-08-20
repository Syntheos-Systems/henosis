/** Interaction tests for atomic roster workflow feedback and recovery controls. */
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  applyAgentControlAction,
  createAgentControlState,
} from "../domain/agentControl";
import type {
  AgentControlState,
  AgentControlValidationIssue,
} from "../domain/agentControl";
import {
  createFixtureAgentCatalog,
  createFixtureAgentIdentities,
  createFixtureAgentRosters,
} from "../data/fixtureRooms";
import { DashboardApplyBar } from "./DashboardApplyBar";

/** Build one fixture reducer state for the primary room roster. */
function controlState(): AgentControlState {
  const roster = createFixtureAgentRosters().find(
    (candidate) => candidate.serverId === "server-henosis",
  );
  if (!roster) {
    throw new Error("The Apply bar tests require the primary fixture roster.");
  }
  return createAgentControlState({
    currentHumanId: "fixture-user",
    canManageRoom: true,
    identities: createFixtureAgentIdentities(),
    catalog: createFixtureAgentCatalog(),
    snapshot: roster,
  });
}

/** Render one Apply bar with no-op callbacks and narrow overrides. */
function renderApplyBar(
  control: AgentControlState,
  overrides: Partial<{
    validationIssues: readonly AgentControlValidationIssue[];
    errorMessage: string | null;
    saving: boolean;
    activationTimedOut: boolean;
    onApply: () => void;
    onDiscard: () => void;
    onRetryActivation: () => void;
    onRefreshActivation: () => void;
  }> = {},
): void {
  render(
    <DashboardApplyBar
      control={control}
      validationIssues={overrides.validationIssues ?? []}
      errorMessage={overrides.errorMessage ?? null}
      saving={overrides.saving ?? false}
      activationTimedOut={overrides.activationTimedOut ?? false}
      onApply={overrides.onApply ?? vi.fn()}
      onDiscard={overrides.onDiscard ?? vi.fn()}
      onRetryActivation={overrides.onRetryActivation ?? vi.fn()}
      onRefreshActivation={overrides.onRefreshActivation ?? vi.fn()}
    />,
  );
}

describe("DashboardApplyBar", () => {
  it("stays absent for clean active truth and exposes Apply and Discard when dirty", () => {
    const clean = controlState();
    const onApply = vi.fn();
    const onDiscard = vi.fn();
    const { rerender } = render(
      <DashboardApplyBar
        control={clean}
        validationIssues={[]}
        errorMessage={null}
        saving={false}
        activationTimedOut={false}
        onApply={onApply}
        onDiscard={onDiscard}
        onRetryActivation={vi.fn()}
        onRefreshActivation={vi.fn()}
      />,
    );
    expect(screen.queryByLabelText("Agent roster changes")).not.toBeInTheDocument();

    const dirty = applyAgentControlAction(clean, {
      type: "setSetting",
      seatId: "seat-mira",
      settingId: "effort",
      value: "high",
    });
    rerender(
      <DashboardApplyBar
        control={dirty}
        validationIssues={[]}
        errorMessage={null}
        saving={false}
        activationTimedOut={false}
        onApply={onApply}
        onDiscard={onDiscard}
        onRetryActivation={vi.fn()}
        onRefreshActivation={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
    fireEvent.click(screen.getByRole("button", { name: "Discard changes" }));
    expect(onApply).toHaveBeenCalledOnce();
    expect(onDiscard).toHaveBeenCalledOnce();
  });

  it("reports validation without hiding the recoverable dirty draft", () => {
    const dirty = applyAgentControlAction(controlState(), {
      type: "setHarness",
      seatId: "seat-mira",
      harnessKey: "claude-code",
    });
    renderApplyBar(dirty, {
      validationIssues: [
        {
          code: "model_required",
          seatId: "seat-mira",
          field: "modelKey",
          message: "Choose an available model.",
        },
      ],
    });

    expect(screen.getByRole("alert")).toHaveTextContent(
      "1 issue is attached to the affected agent seats.",
    );
    expect(screen.getByRole("button", { name: "Apply roster" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Discard changes" })).toBeEnabled();
  });

  it("preserves a conflict for value-free field review and offers only discard", () => {
    const base = controlState();
    const local = applyAgentControlAction(base, {
      type: "setSetting",
      seatId: "seat-mira",
      settingId: "effort",
      value: "high",
    });
    const remote = {
      ...base.serverSnapshot,
      desiredRevision: 3,
      seats: base.serverSnapshot.seats.map((seat) =>
        seat.seatId === "seat-mira"
          ? { ...seat, enabled: false, configurationRevision: 3 }
          : { ...seat, configurationRevision: 3 },
      ),
    };
    const conflicted = applyAgentControlAction(local, {
      type: "revisionConflict",
      snapshot: remote,
    });
    const onDiscard = vi.fn();
    renderApplyBar(conflicted, { onDiscard });

    expect(screen.queryByRole("button", { name: "Apply roster" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Review changes" }));
    expect(screen.getByText("Mine: Setting: effort")).toBeInTheDocument();
    expect(screen.getByText("Server: Participation")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Discard mine" }));
    expect(onDiscard).toHaveBeenCalledOnce();
  });

  it("offers explicit pending timeout refresh and failed activation retry", () => {
    const pending = applyAgentControlAction(controlState(), {
      type: "runtimeUpdated",
      status: {
        paused: false,
        desiredRevision: 3,
        activeRevision: 2,
        lastGoodRevision: 2,
        runtimeActivation: "pending",
        runtimeErrorCode: null,
        runtimeErrorMessage: null,
      },
    });
    const onRefresh = vi.fn();
    const { rerender } = render(
      <DashboardApplyBar
        control={pending}
        validationIssues={[]}
        errorMessage={null}
        saving={false}
        activationTimedOut
        onApply={vi.fn()}
        onDiscard={vi.fn()}
        onRetryActivation={vi.fn()}
        onRefreshActivation={onRefresh}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Refresh status" }));
    expect(onRefresh).toHaveBeenCalledOnce();

    const failed = applyAgentControlAction(pending, {
      type: "runtimeUpdated",
      status: {
        paused: false,
        desiredRevision: 3,
        activeRevision: 2,
        lastGoodRevision: 2,
        runtimeActivation: "failed",
        runtimeErrorCode: "bridge_start_failed",
        runtimeErrorMessage: "The room bridge did not start.",
      },
    });
    const onRetry = vi.fn();
    rerender(
      <DashboardApplyBar
        control={failed}
        validationIssues={[]}
        errorMessage={null}
        saving={false}
        activationTimedOut={false}
        onApply={vi.fn()}
        onDiscard={vi.fn()}
        onRetryActivation={onRetry}
        onRefreshActivation={vi.fn()}
      />,
    );
    expect(screen.getByText("Last good revision 2")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry activation" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });
});
