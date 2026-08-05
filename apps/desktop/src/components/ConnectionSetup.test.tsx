/** Interaction tests for the first-run Rift connection form. */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HenosisClientError } from "../services/henosisClient";
import { ConnectionSetup } from "./ConnectionSetup";

describe("ConnectionSetup", () => {
  it("submits labeled credentials through the supplied native callback", async () => {
    const onConnect = vi.fn().mockResolvedValue(undefined);
    render(<ConnectionSetup busy={false} onConnect={onConnect} />);

    fireEvent.change(screen.getByLabelText("Rift endpoint"), {
      target: { value: "https://rift.example.test/" },
    });
    fireEvent.change(screen.getByLabelText("Username"), {
      target: { value: "operator" },
    });
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: "correct horse battery staple" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect and open rooms" }));

    expect(onConnect).toHaveBeenCalledWith({
      endpoint: "https://rift.example.test/",
      username: "operator",
      password: "correct horse battery staple",
    });
  });

  it("starts blank, explains the prerequisite, and exposes visible setup progress", () => {
    render(<ConnectionSetup busy={false} onConnect={vi.fn()} />);

    expect(screen.getByLabelText("Rift endpoint")).toHaveValue("");
    const progress = screen.getByLabelText("Setup progress");
    expect(progress).toHaveTextContent("Install");
    expect(progress).toHaveTextContent("Connect");
    expect(progress).toHaveTextContent("Rooms");
    expect(screen.getByText("Connect").closest("li")).toHaveAttribute(
      "aria-current",
      "step",
    );
    expect(screen.getByText(/does not install or start Rift/)).toBeInTheDocument();
  });

  it("fills only the real listener default for an already-running local Rift", () => {
    render(<ConnectionSetup busy={false} onConnect={vi.fn()} />);

    fireEvent.click(
      screen.getByRole("button", { name: "Use an already-running local Rift" }),
    );

    expect(screen.getByLabelText("Rift endpoint")).toHaveValue(
      "http://127.0.0.1:3200",
    );
    expect(screen.getByLabelText("Rift endpoint")).toHaveFocus();
  });

  it("prefills only saved non-secret profile fields", () => {
    render(
      <ConnectionSetup
        profile={{ endpoint: "https://rift.example.test/", username: "operator" }}
        busy={false}
        onConnect={vi.fn()}
      />,
    );

    expect(screen.getByLabelText("Rift endpoint")).toHaveValue(
      "https://rift.example.test/",
    );
    expect(screen.getByLabelText("Username")).toHaveValue("operator");
    expect(screen.getByLabelText("Password")).toHaveValue("");
  });

  it("attaches network failures to the endpoint and focuses its recovery control", () => {
    render(
      <ConnectionSetup
        busy={false}
        error={new HenosisClientError("network", "Henosis could not reach Rift.")}
        onConnect={vi.fn()}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Henosis could not reach Rift.",
    );
    expect(screen.getByLabelText("Rift endpoint")).toHaveAttribute(
      "aria-describedby",
      "endpoint-help endpoint-error",
    );
    expect(screen.getByLabelText("Rift endpoint")).toHaveAttribute(
      "aria-invalid",
      "true",
    );
    expect(screen.getByLabelText("Rift endpoint")).toHaveFocus();
  });

  it("attaches authentication failures to the account fields and focuses password", () => {
    render(
      <ConnectionSetup
        busy={false}
        error={new HenosisClientError("authentication", "Rift rejected that account.")}
        onConnect={vi.fn()}
      />,
    );

    expect(screen.getByLabelText("Username")).toHaveAttribute(
      "aria-describedby",
      "account-error",
    );
    expect(screen.getByLabelText("Password")).toHaveAttribute(
      "aria-invalid",
      "true",
    );
    expect(screen.getByLabelText("Password")).toHaveFocus();
  });

  it("focuses the safe form alert for storage and protocol recovery", () => {
    render(
      <ConnectionSetup
        busy={false}
        error={new HenosisClientError("storage", "Henosis could not save the profile.")}
        onConnect={vi.fn()}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Henosis could not save the profile.",
    );
    expect(screen.getByRole("alert")).toHaveFocus();
  });

  it("clears only the password after a rejected submission", async () => {
    const onConnect = vi
      .fn()
      .mockRejectedValue(new HenosisClientError("authentication", "Rejected"));
    render(<ConnectionSetup busy={false} onConnect={onConnect} />);

    fireEvent.change(screen.getByLabelText("Rift endpoint"), {
      target: { value: "https://rift.example.test" },
    });
    fireEvent.change(screen.getByLabelText("Username"), {
      target: { value: "operator" },
    });
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: "rejected-password" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect and open rooms" }));

    await waitFor(() => expect(screen.getByLabelText("Password")).toHaveValue(""));
    expect(screen.getByLabelText("Rift endpoint")).toHaveValue(
      "https://rift.example.test",
    );
    expect(screen.getByLabelText("Username")).toHaveValue("operator");
  });
});
