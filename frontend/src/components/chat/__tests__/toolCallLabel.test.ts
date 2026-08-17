import { describe, it, expect } from "vitest";
import { describeToolCall, isAgentOutputPath } from "../toolCallLabel";

// ---------------------------------------------------------------------------
// isAgentOutputPath
// ---------------------------------------------------------------------------

describe("isAgentOutputPath", () => {
  it("matches workspace task output paths", () => {
    expect(isAgentOutputPath("/workspace/tasks/abc-123/output.txt")).toBe(true);
    expect(isAgentOutputPath("/tasks/d90b8b44-4321/output")).toBe(true);
    expect(isAgentOutputPath("/foo/tasks/some-id/output.md")).toBe(true);
  });

  it("returns false for unrelated paths", () => {
    expect(isAgentOutputPath("/home/user/tasks/readme.md")).toBe(false);
    expect(isAgentOutputPath("/tasks/nooutput")).toBe(false);
    expect(isAgentOutputPath("/src/components/chat/foo.ts")).toBe(false);
    expect(isAgentOutputPath("")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — Read (input-varying)
// ---------------------------------------------------------------------------

describe("describeToolCall — Read", () => {
  it("agent output path → 'Reading agent output'", () => {
    expect(
      describeToolCall("Read", { file_path: "/workspace/tasks/abc-123/output.txt" }).label,
    ).toBe("Reading agent output");
  });

  it("regular file → 'Reading <filename>'", () => {
    expect(describeToolCall("Read", { file_path: "/src/lib/foo.ts" }).label).toBe("Reading foo.ts");
  });

  it("no input → 'Reading'", () => {
    expect(describeToolCall("Read").label).toBe("Reading");
  });

  it("MCP-namespaced Read works the same", () => {
    expect(
      describeToolCall("mcp__launchpad__Read", { file_path: "/project/docs/roadmap.md" }).label,
    ).toBe("Reading roadmap.md");
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — Edit
// ---------------------------------------------------------------------------

describe("describeToolCall — Edit", () => {
  it("regular file → 'Editing <filename>'", () => {
    expect(describeToolCall("Edit", { file_path: "/src/lib/foo.ts" }).label).toBe("Editing foo.ts");
  });

  it("no input → 'Editing'", () => {
    expect(describeToolCall("Edit").label).toBe("Editing");
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — Write (verb is 'Creating')
// ---------------------------------------------------------------------------

describe("describeToolCall — Write", () => {
  it("regular file → 'Creating <filename>'", () => {
    expect(describeToolCall("Write", { file_path: "/src/lib/newfile.ts" }).label).toBe(
      "Creating newfile.ts",
    );
  });

  it("no input → 'Creating'", () => {
    expect(describeToolCall("Write").label).toBe("Creating");
  });

  it("uses 'Creating' verb (not 'Writing') for all Write paths", () => {
    expect(describeToolCall("Write", { file_path: "/any/path/file.ts" }).label).toMatch(/^Creating/);
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — Bash
// ---------------------------------------------------------------------------

describe("describeToolCall — Bash", () => {
  it("description present → 'Running: <description>'", () => {
    expect(
      describeToolCall("Bash", { description: "Install dependencies", command: "npm install" }).label,
    ).toBe("Running: Install dependencies");
  });

  it("no description → 'Running <command>'", () => {
    expect(describeToolCall("Bash", { command: "npm test" }).label).toBe("Running npm test");
  });

  it("no input → 'Running'", () => {
    expect(describeToolCall("Bash").label).toBe("Running");
  });

  it("truncates long descriptions at 28 chars", () => {
    const long = "A".repeat(40);
    const result = describeToolCall("Bash", { description: long }).label;
    expect(result).toBe(`Running: ${"A".repeat(25)}…`);
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — Grep
// ---------------------------------------------------------------------------

describe("describeToolCall — Grep", () => {
  it("'Searching for <pattern>'", () => {
    expect(describeToolCall("Grep", { pattern: "describeToolCall" }).label).toBe(
      "Searching for describeToolCall",
    );
  });

  it("no input → 'Searching'", () => {
    expect(describeToolCall("Grep").label).toBe("Searching");
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — Glob
// ---------------------------------------------------------------------------

describe("describeToolCall — Glob", () => {
  it("'Finding files: <pattern>'", () => {
    expect(describeToolCall("Glob", { pattern: "**/*.ts" }).label).toBe("Finding files: **/*.ts");
  });

  it("no input → 'Finding files'", () => {
    expect(describeToolCall("Glob").label).toBe("Finding files");
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — WebSearch
// ---------------------------------------------------------------------------

describe("describeToolCall — WebSearch", () => {
  it("query present → 'Searching the web: <query>'", () => {
    expect(describeToolCall("WebSearch", { query: "rust async" }).label).toBe(
      "Searching the web: rust async",
    );
  });

  it("no query → 'Searching the web'", () => {
    expect(describeToolCall("WebSearch").label).toBe("Searching the web");
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — WebFetch
// ---------------------------------------------------------------------------

describe("describeToolCall — WebFetch", () => {
  it("extracts hostname from url → 'Fetching <domain>'", () => {
    expect(describeToolCall("WebFetch", { url: "https://docs.rs/tokio/latest/" }).label).toBe(
      "Fetching docs.rs",
    );
  });

  it("no url → 'Fetching'", () => {
    expect(describeToolCall("WebFetch").label).toBe("Fetching");
  });

  it("invalid url falls back to 'Fetching'", () => {
    expect(describeToolCall("WebFetch", { url: "not-a-url" }).label).toBe("Fetching");
  });
});

// ---------------------------------------------------------------------------
// describeToolCall — ListDirectory
// ---------------------------------------------------------------------------

describe("describeToolCall — ListDirectory", () => {
  it("'Browsing <dir>'", () => {
    expect(describeToolCall("ListDirectory", { path: "/src/components" }).label).toBe(
      "Browsing components",
    );
  });

  it("no input → 'Browsing'", () => {
    expect(describeToolCall("ListDirectory").label).toBe("Browsing");
  });
});

// ---------------------------------------------------------------------------
// Regression: workflow-chat path yields same label as main chat after unification
// ---------------------------------------------------------------------------

describe("regression — workflow-chat drift guard", () => {
  it("Delegate: shared describeToolCall returns target-aware label (not old stale 'Using Delegate')", () => {
    // Old PhaseChat local copy was missing the Delegate branch — would fall
    // through to nameMap and return "Using Delegate". The shared module must
    // return the correct label for both surfaces.
    expect(describeToolCall("Delegate", { target: "Reviewer" }).label).toBe(
      "Delegating to Reviewer…",
    );
    expect(describeToolCall("Delegate", { target: "Reviewer" }, true).label).toBe(
      "Delegated to Reviewer",
    );
  });

  it("RunSkill: shared describeToolCall returns skill-aware label (not stale 'Using RunSkill')", () => {
    // Old PhaseChat local copy was missing the RunSkill branch.
    expect(describeToolCall("RunSkill", { skill: "simplify" }).label).toBe(
      "Loading skill: simplify",
    );
    expect(describeToolCall("RunSkill").label).toBe("Loading skill");
  });

  it("MCP-routed Delegate and RunSkill also get enriched labels in both surfaces", () => {
    expect(
      describeToolCall("mcp__launchpad__Delegate", { target: "Developer" }).label,
    ).toBe("Delegating to Developer…");
    expect(
      describeToolCall("mcp__launchpad__RunSkill", { skill: "review" }).label,
    ).toBe("Loading skill: review");
  });
});
