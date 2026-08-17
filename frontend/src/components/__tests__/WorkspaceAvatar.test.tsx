// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

import { WorkspaceAvatar } from "../WorkspaceAvatar";

describe("WorkspaceAvatar", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(props: React.ComponentProps<typeof WorkspaceAvatar>) {
    await act(async () => {
      root.render(React.createElement(WorkspaceAvatar, props));
    });
  }

  function rootBox(): HTMLDivElement {
    const el = container.firstElementChild;
    if (!el) throw new Error("WorkspaceAvatar did not render a root element");
    return el as HTMLDivElement;
  }

  describe("emoji set", () => {
    it("renders the emoji alone, with no background colour applied", async () => {
      await render({ name: "Client Project", path: "/Users/x/client", emoji: "🚀", color: "#3B82F6", size: 36 });
      expect(container.textContent).toBe("🚀");
      // No filled rounded rect — the box itself must carry no background.
      expect(rootBox().style.backgroundColor).toBe("");
    });

    it("treats an emoji that's only whitespace as unset (falls back to the letter tile)", async () => {
      await render({ name: "Client Project", emoji: "   ", color: "#3B82F6", size: 36 });
      expect(container.textContent).toBe("C");
      expect(rootBox().style.backgroundColor).toBe("rgb(59, 130, 246)");
    });
  });

  describe("emoji unset", () => {
    it("renders the background colour and the correct uppercase letter from name", async () => {
      await render({ name: "client project", path: "/Users/x/client", emoji: null, color: "#3B82F6", size: 36 });
      expect(rootBox().style.backgroundColor).toBe("rgb(59, 130, 246)");
      expect(container.textContent).toBe("C");
    });

    it("treats an absent emoji prop the same as null", async () => {
      await render({ name: "Zebra", color: "#22C55E", size: 36 });
      expect(rootBox().style.backgroundColor).toBe("rgb(34, 197, 94)");
      expect(container.textContent).toBe("Z");
    });

    it("falls back to the dotted path basename when name is absent, and never renders '.'", async () => {
      await render({ name: null, path: "/Users/x/.launchpad_studio-tools", emoji: undefined, color: "#EF4444", size: 36 });
      expect(container.textContent).toBe("L");
      expect(container.textContent).not.toContain(".");
    });

    it("falls back to '?' when neither name nor path yields a usable character", async () => {
      await render({ name: "", path: "", emoji: null, color: "#EF4444", size: 36 });
      expect(container.textContent).toBe("?");
    });

    it("does not split an astral (non-BMP) first character into a broken surrogate half", async () => {
      // U+1F600 GRINNING FACE — outside the BMP, encoded as a surrogate
      // pair in UTF-16. `s[0]` would return only the lead surrogate; this
      // must read the whole code point instead.
      await render({ name: "😀Team", color: "#8B5CF6", size: 36 });
      expect(container.textContent).toBe("😀");
      expect(Array.from(container.textContent ?? "")).toHaveLength(1);
    });
  });
});
