import { describe, it, expect } from "vitest";
import { describeToolCall } from "../StreamingMessage";

describe("describeToolCall — TodoCreate", () => {
  it("returns 'Using TodoList' when pending with no name", () => {
    const { label } = describeToolCall("TodoCreate", undefined, false);
    expect(label).toBe("Using TodoList");
  });

  it("returns 'Using TodoList: <name>' when pending with a name", () => {
    const { label } = describeToolCall("TodoCreate", { name: "Build feature X" }, false);
    expect(label).toBe("Using TodoList: Build feature X");
  });

  it("returns 'Used TodoList' when completed", () => {
    const { label } = describeToolCall("TodoCreate", undefined, true);
    expect(label).toBe("Used TodoList");
  });

  it("returns 'Used TodoList' when completed even if name is provided", () => {
    const { label } = describeToolCall("TodoCreate", { name: "Something" }, true);
    expect(label).toBe("Used TodoList");
  });
});

describe("describeToolCall — async mode unchanged", () => {
  it("does not show a chip label for Bash (generic tool, not TodoCreate)", () => {
    const { label } = describeToolCall("Bash", { command: "ls" }, false);
    expect(label).toBe("Running ls");
  });

  it("Delegate still works as before", () => {
    const { label } = describeToolCall("Delegate", { target: "research-agent" }, false);
    expect(label).toBe("Delegating to research-agent…");
  });
});
