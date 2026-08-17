import { describe, it, expect } from "vitest";
import { describeToolCall } from "../StreamingMessage";

describe("describeToolCall — Memory tool chip wording", () => {
  it("MemoryWrite: pending returns 'Saving memory…'", () => {
    expect(describeToolCall("MemoryWrite").label).toBe("Saving memory…");
  });

  it("MemoryWrite: completed returns 'Saved memory'", () => {
    expect(describeToolCall("MemoryWrite", undefined, true).label).toBe("Saved memory");
  });

  it("MemoryEdit: pending returns 'Editing memory…'", () => {
    expect(describeToolCall("MemoryEdit").label).toBe("Editing memory…");
  });

  it("MemoryEdit: completed returns 'Edited memory'", () => {
    expect(describeToolCall("MemoryEdit", undefined, true).label).toBe("Edited memory");
  });

  it("MemoryDelete: pending returns 'Deleting memory…'", () => {
    expect(describeToolCall("MemoryDelete").label).toBe("Deleting memory…");
  });

  it("MemoryDelete: completed returns 'Deleted memory'", () => {
    expect(describeToolCall("MemoryDelete", undefined, true).label).toBe("Deleted memory");
  });

  it("MemoryList: pending returns 'Listing memories…'", () => {
    expect(describeToolCall("MemoryList").label).toBe("Listing memories…");
  });

  it("MemoryList: completed returns 'Listed memories'", () => {
    expect(describeToolCall("MemoryList", undefined, true).label).toBe("Listed memories");
  });

  it("MCP-namespaced Memory tools strip prefix and match correctly", () => {
    expect(describeToolCall("mcp__launchpad__MemoryWrite").label).toBe("Saving memory…");
    expect(describeToolCall("mcp__launchpad__MemoryList", undefined, true).label).toBe("Listed memories");
  });
});
