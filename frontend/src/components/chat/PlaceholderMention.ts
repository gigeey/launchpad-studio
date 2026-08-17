import { Node } from "@tiptap/core";
import { PluginKey } from "@tiptap/pm/state";
import Suggestion, { type SuggestionOptions } from "@tiptap/suggestion";
import { getPlaceholder } from "../../data/systemPromptPlaceholders";

export const PlaceholderMentionPluginKey = new PluginKey(
  "placeholderMentionSuggestion",
);

export type PlaceholderMentionOptions = {
  /**
   * Suggestion configuration used to trigger the autocomplete popover. The
   * consumer (SystemPromptEditor) supplies a `render` object to drive popover
   * state; sensible defaults (trigger, command, allowedPrefixes) live here so
   * the extension remains usable standalone.
   */
  suggestion: Omit<SuggestionOptions, "editor">;
};

/**
 * Atomic inline TipTap node representing a `{{placeholder}}` token in the
 * system prompt editor.
 *
 * Autocomplete trigger: we use `@tiptap/suggestion` directly with `char: "{{"`.
 * Suggestion runs the `char` string through `escapeForRegEx` before assembling
 * the match regex, so multi-character prefixes work as triggers — no custom
 * input-rule or ProseMirror key-handler is required.
 */
export const PlaceholderMention = Node.create<PlaceholderMentionOptions>({
  name: "placeholderMention",
  group: "inline",
  inline: true,
  atom: true,
  selectable: true,

  addOptions() {
    return {
      suggestion: {
        char: "{{",
        pluginKey: PlaceholderMentionPluginKey,
        allowSpaces: false,
        allowedPrefixes: null,
        // Non-empty stub so Suggestion keeps the popover active while the
        // consumer filters PLACEHOLDERS in the React popover (matches the
        // pattern used by MentionAutocomplete / ChatInput).
        items: () => [1],
        command: ({ editor, range, props }) => {
          const id = (props as { id: string }).id;
          editor
            .chain()
            .focus()
            .insertContentAt(range, {
              type: "placeholderMention",
              attrs: { id },
            })
            .run();
        },
        render: () => ({}),
      },
    };
  },

  addAttributes() {
    return {
      id: {
        default: null,
        parseHTML: (element: HTMLElement) =>
          element.getAttribute("data-placeholder-id"),
        renderHTML: (attributes: Record<string, string>) => {
          if (!attributes.id) return {};
          return { "data-placeholder-id": attributes.id };
        },
      },
    };
  },

  parseHTML() {
    return [
      {
        tag: 'span[data-type="placeholder-mention"]',
      },
    ];
  },

  renderHTML({ node, HTMLAttributes }) {
    const placeholder = getPlaceholder(node.attrs.id);
    const attrs: Record<string, string> = {
      ...HTMLAttributes,
      "data-type": "placeholder-mention",
      class: "placeholder-chip",
    };
    // Native browser tooltip: shows on hover, dismisses on mouseleave, and
    // never interferes with caret placement inside the contenteditable.
    // Unknown ids (shouldn't occur post-parser, but defensive) get no title.
    if (placeholder) {
      attrs.title = placeholder.description;
    }
    return ["span", attrs, `{{${node.attrs.id}}}`];
  },

  renderText({ node }) {
    return `{{${node.attrs.id}}}`;
  },

  addProseMirrorPlugins() {
    return [
      Suggestion({
        editor: this.editor,
        ...this.options.suggestion,
      }),
    ];
  },
});
