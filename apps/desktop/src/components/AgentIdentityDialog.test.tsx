/** Interaction coverage for identity selection, creation, and explicit claim. */
import { useRef, useState } from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { OwnedAgentIdentity, UnownedAgentIdentity } from "../domain/agentControl";
import { HenosisClientError } from "../services/henosisClient";
import { AgentIdentityDialog } from "./AgentIdentityDialog";
import type { AgentIdentityDialogProps } from "./AgentIdentityDialog";

/** Existing owned identity available to add to the current room. */
const OWNED_IDENTITY: OwnedAgentIdentity = {
  id: "agent-lumen",
  username: "lumen",
  displayName: "Lumen",
  ownerUserId: "fixture-user",
};

/** Roster-visible imported identity available for an explicit manager claim. */
const UNOWNED_IDENTITY: UnownedAgentIdentity = {
  id: "agent-imported",
  username: "imported",
  displayName: "Imported scout",
  ownerUserId: null,
};

/** Callback overrides accepted by the stateful dialog test harness. */
type DialogOverrides = Omit<
  AgentIdentityDialogProps,
  "onClose" | "returnFocusRef"
>;

/** Render the dialog behind its real trigger and own its open state. */
function DialogHarness(overrides: Partial<DialogOverrides>) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const props: DialogOverrides = {
    ownedIdentities: [OWNED_IDENTITY],
    unownedIdentities: [UNOWNED_IDENTITY],
    canClaim: true,
    onSelectIdentity: vi.fn(),
    onCreateIdentity: vi.fn(async () => OWNED_IDENTITY),
    onClaimIdentity: vi.fn(async () => ({
      ...UNOWNED_IDENTITY,
      ownerUserId: "fixture-user",
    })),
    onIdentityMutated: vi.fn(async () => undefined),
    ...overrides,
  };

  return (
    <>
      <button ref={triggerRef} type="button" onClick={() => setOpen(true)}>
        Add agent identity
      </button>
      {open ? (
        <AgentIdentityDialog
          {...props}
          returnFocusRef={triggerRef}
          onClose={() => setOpen(false)}
        />
      ) : null}
    </>
  );
}

describe("AgentIdentityDialog", () => {
  it("keeps owned and unowned identities distinct and restores trigger focus", () => {
    const onSelectIdentity = vi.fn();
    render(<DialogHarness onSelectIdentity={onSelectIdentity} />);
    const trigger = screen.getByRole("button", { name: "Add agent identity" });

    fireEvent.click(trigger);
    expect(screen.getByRole("heading", { name: "Add an agent identity" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Your identities" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Needs an owner" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Add Lumen to room" }));

    expect(onSelectIdentity).toHaveBeenCalledWith("agent-lumen");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("validates server-aligned public fields before creating and refreshing", async () => {
    const created: OwnedAgentIdentity = {
      id: "agent-builder",
      username: "builder",
      displayName: "Builder",
      ownerUserId: "fixture-user",
    };
    const onCreateIdentity = vi.fn(async () => created);
    const onIdentityMutated = vi.fn(async () => undefined);
    render(
      <DialogHarness
        onCreateIdentity={onCreateIdentity}
        onIdentityMutated={onIdentityMutated}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
    const handle = screen.getByRole("textbox", { name: "Handle" });
    const displayName = screen.getByRole("textbox", { name: "Display name (optional)" });
    const create = screen.getByRole("button", { name: "Create and add" });

    fireEvent.click(create);
    expect(screen.getByText("Handle must be between 3 and 32 UTF-8 bytes.")).toBeInTheDocument();

    fireEvent.change(handle, { target: { value: "123456789012345678901234567890123" } });
    fireEvent.click(create);
    expect(screen.getByText("Handle must be between 3 and 32 UTF-8 bytes.")).toBeInTheDocument();

    fireEvent.change(handle, { target: { value: "builder" } });
    fireEvent.change(displayName, { target: { value: "B".repeat(65) } });
    fireEvent.click(create);
    expect(screen.getByText("Display name must be 64 characters or fewer.")).toBeInTheDocument();

    fireEvent.change(displayName, { target: { value: "Builder" } });
    fireEvent.click(create);

    await waitFor(() => {
      expect(onCreateIdentity).toHaveBeenCalledWith("builder", "Builder");
      expect(onIdentityMutated).toHaveBeenCalledWith(created, true);
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("requires explicit confirmation before claiming a roster-visible identity", async () => {
    const claimed: OwnedAgentIdentity = {
      ...UNOWNED_IDENTITY,
      ownerUserId: "fixture-user",
    };
    const onClaimIdentity = vi.fn(async () => claimed);
    const onIdentityMutated = vi.fn(async () => undefined);
    render(
      <DialogHarness
        onClaimIdentity={onClaimIdentity}
        onIdentityMutated={onIdentityMutated}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
    fireEvent.click(screen.getByRole("button", { name: "Claim Imported scout" }));
    expect(onClaimIdentity).not.toHaveBeenCalled();
    expect(screen.getByText(/Claiming makes you the persistent owner/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Confirm claim" }));
    await waitFor(() => {
      expect(onClaimIdentity).toHaveBeenCalledWith("agent-imported");
      expect(onIdentityMutated).toHaveBeenCalledWith(claimed, false);
    });
  });

  it("keeps the dialog open with a normalized claim error", async () => {
    const onClaimIdentity = vi.fn(async () => {
      throw new HenosisClientError(
        "forbidden",
        "Only a room manager can claim this identity.",
      );
    });
    render(<DialogHarness onClaimIdentity={onClaimIdentity} />);

    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
    fireEvent.click(screen.getByRole("button", { name: "Claim Imported scout" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm claim" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Only a room manager can claim this identity.",
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("explains and disables claim when the current human is not a room manager", () => {
    render(<DialogHarness canClaim={false} />);

    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));

    expect(screen.getByText("A room manager must claim imported identities.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Claim Imported scout" })).toBeDisabled();
  });

  it("retries only refresh after a mutation already succeeded", async () => {
    const created: OwnedAgentIdentity = {
      id: "agent-builder",
      username: "builder",
      displayName: null,
      ownerUserId: "fixture-user",
    };
    const onCreateIdentity = vi.fn(async () => created);
    const onIdentityMutated = vi
      .fn()
      .mockRejectedValueOnce(new HenosisClientError("network", "Refresh failed."))
      .mockResolvedValueOnce(undefined);
    render(
      <DialogHarness
        onCreateIdentity={onCreateIdentity}
        onIdentityMutated={onIdentityMutated}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Handle" }), {
      target: { value: "builder" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create and add" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("Refresh failed.");
    expect(
      screen.getByRole("button", { name: "Close agent identity dialog" }),
    ).toBeDisabled();
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry refresh" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(onCreateIdentity).toHaveBeenCalledOnce();
    expect(onIdentityMutated).toHaveBeenCalledTimes(2);
  });

  it("closes on Escape and restores focus without closing an ancestor surface", () => {
    render(<DialogHarness />);
    const trigger = screen.getByRole("button", { name: "Add agent identity" });

    fireEvent.click(trigger);
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });
});
