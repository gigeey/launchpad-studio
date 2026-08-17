import Mention from "@tiptap/extension-mention";

/**
 * Custom TipTap Mention node extension for chat input.
 *
 * Renders @mentions as styled atomic chip elements.
 * - Stores `id` (agent_id) and `label` (display name)
 * - renderHTML outputs a styled <span> chip showing "@label"
 * - renderText outputs "@id" so plain-text serialization produces the @agent_id format the backend expects
 */
export const MentionExtension = Mention.extend({
  // Ensure the node is inline and atomic (cursor cannot enter)
  inline: true,
  group: "inline",
  atom: true,

  addAttributes() {
    return {
      id: {
        default: null,
        parseHTML: (element: HTMLElement) => element.getAttribute("data-id"),
        renderHTML: (attributes: Record<string, string>) => {
          if (!attributes.id) return {};
          return { "data-id": attributes.id };
        },
      },
      label: {
        default: null,
        parseHTML: (element: HTMLElement) => element.getAttribute("data-label"),
        renderHTML: (attributes: Record<string, string>) => {
          if (!attributes.label) return {};
          return { "data-label": attributes.label };
        },
      },
    };
  },

  renderHTML({ node, HTMLAttributes }) {
    return [
      "span",
      {
        ...HTMLAttributes,
        class: "mention-chip",
        "data-type": "mention",
      },
      `@${node.attrs.label ?? node.attrs.id}`,
    ];
  },

  renderText({ node }) {
    // Plain-text serialization: output @agent_id for the backend
    return `@${node.attrs.id}`;
  },
});
