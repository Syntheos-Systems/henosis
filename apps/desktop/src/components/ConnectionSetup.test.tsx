/** Interaction tests for the first-run Rift connection form. */
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ConnectionSetup } from "./ConnectionSetup";

describe("ConnectionSetup", () => {
  it("submits labeled credentials through the supplied native callback", async () => {
    const onConnect = vi.fn().mockResolvedValue(undefined);
    render(<ConnectionSetup busy={false} onConnect={onConnect} />);

    fireEvent.change(screen.getByLabelText("Rift endpoint"), {
      target: { value: "https://rift.example.test/" },
    });
    fireEvent.change(screen.getByLabelText("Username"), {
      target: { value: "zan" },
    });
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: "correct horse battery staple" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect and view rooms" }));

    expect(onConnect).toHaveBeenCalledWith({
      endpoint: "https://rift.example.test/",
      username: "zan",
      password: "correct horse battery staple",
    });
  });

  it("renders safe native errors as an alert", () => {
    render(
      <ConnectionSetup
        busy={false}
        error="Rift did not accept that username and password."
        onConnect={vi.fn()}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Rift did not accept that username and password.",
    );
  });
});
