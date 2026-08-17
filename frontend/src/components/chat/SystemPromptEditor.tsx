import { useCallback, useEffect, useRef, useState } from "react";
import { useEditor, EditorContent } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import Placeholder from "@tiptap/extension-placeholder";
import { exitSuggestion } from "@tiptap/suggestion";

import {
    PlaceholderMention,
    PlaceholderMentionPluginKey,
} from "./PlaceholderMention";
import { PlaceholderAutocomplete } from "./PlaceholderAutocomplete";
import { PLACEHOLDER_REGEX } from "../../data/systemPromptPlaceholders";

type SystemPromptEditorProps = {
    id?: string;
    value: string;
    onChange: (text: string) => void;
    placeholder?: string;
    fill?: boolean;
    readOnly?: boolean;
};

type DocNode =
    | { type: "text"; text: string }
    | { type: "placeholderMention"; attrs: { id: string } };

type DocParagraph = { type: "paragraph"; content?: DocNode[] };

type DocRoot = { type: "doc"; content: DocParagraph[] };

type SuggestionCommand = (attrs: { id: string }) => void;

/**
 * Parse a plaintext system prompt into a ProseMirror JSON doc. Known
 * `{{placeholder}}` tokens become atomic `placeholderMention` nodes; newlines
 * become paragraph breaks; everything else — including unknown `{{foo}}`
 * tokens — stays as literal text runs.
 */
function stringToDoc(value: string): DocRoot {
    const lines = value.split("\n");
    const paragraphs: DocParagraph[] = lines.map((line) => {
        if (line.length === 0) {
            return { type: "paragraph" };
        }
        const nodes: DocNode[] = [];
        // Fresh regex each call — PLACEHOLDER_REGEX is global, so reusing it
        // would leak `lastIndex` across invocations.
        const re = new RegExp(PLACEHOLDER_REGEX.source, "g");
        let lastIndex = 0;
        let match: RegExpExecArray | null;
        while ((match = re.exec(line)) !== null) {
            if (match.index > lastIndex) {
                nodes.push({ type: "text", text: line.slice(lastIndex, match.index) });
            }
            nodes.push({ type: "placeholderMention", attrs: { id: match[1] } });
            lastIndex = match.index + match[0].length;
        }
        if (lastIndex < line.length) {
            nodes.push({ type: "text", text: line.slice(lastIndex) });
        }
        return { type: "paragraph", content: nodes };
    });
    return {
        type: "doc",
        content: paragraphs.length > 0 ? paragraphs : [{ type: "paragraph" }],
    };
}

/**
 * Plaintext system-prompt editor that renders known `{{placeholder}}` tokens
 * as inline pills. `editor.getText({ blockSeparator: "\n" })` round-trips
 * byte-for-byte with the saved `system_prompt` string.
 */
export function SystemPromptEditor({
    id,
    value,
    onChange,
    placeholder,
    fill,
    readOnly,
}: SystemPromptEditorProps) {
    const [suggestionActive, setSuggestionActive] = useState(false);
    const [suggestionQuery, setSuggestionQuery] = useState("");
    const [suggestionRect, setSuggestionRect] = useState<DOMRect | null>(null);
    const suggestionCommandRef = useRef<SuggestionCommand | null>(null);

    const editor = useEditor({
        extensions: [
            StarterKit.configure({
                // Plaintext-only: disable all block + inline formatting.
                blockquote: false,
                bulletList: false,
                codeBlock: false,
                heading: false,
                horizontalRule: false,
                listItem: false,
                orderedList: false,
                code: false,
                bold: false,
                italic: false,
                strike: false,
            }),
            Placeholder.configure({ placeholder: placeholder ?? "" }),
            PlaceholderMention.configure({
                suggestion: {
                    render: () => ({
                        onStart: (props) => {
                            setSuggestionActive(true);
                            setSuggestionQuery(props.query);
                            setSuggestionRect(props.clientRect?.() ?? null);
                            suggestionCommandRef.current = (attrs) =>
                                props.command(attrs);
                        },
                        onUpdate: (props) => {
                            setSuggestionQuery(props.query);
                            setSuggestionRect(props.clientRect?.() ?? null);
                            suggestionCommandRef.current = (attrs) =>
                                props.command(attrs);
                        },
                        onExit: () => {
                            setSuggestionActive(false);
                            setSuggestionQuery("");
                            setSuggestionRect(null);
                            suggestionCommandRef.current = null;
                        },
                        // PlaceholderAutocomplete handles navigation via a
                        // capture-phase window listener; defer to it.
                        onKeyDown: () => false,
                    }),
                },
            }),
        ],
        content: stringToDoc(value),
        editable: !readOnly,
        editorProps: {
            attributes: {
                id: id ?? "",
                class:
                    "w-full outline-none text-[14px] text-[var(--text-primary)] leading-relaxed whitespace-pre-wrap break-words",
            },
        },
        onUpdate: ({ editor: ed }) => {
            onChange(ed.getText({ blockSeparator: "\n" }));
        },
    });

    useEffect(() => {
        if (!editor) return;
        const current = editor.getText({ blockSeparator: "\n" });
        if (current !== value) {
            editor.commands.setContent(stringToDoc(value), { emitUpdate: false });
        }
    }, [value, editor]);

    useEffect(() => {
        if (!editor) return;
        const desired = !readOnly;
        if (editor.isEditable !== desired) editor.setEditable(desired);
    }, [readOnly, editor]);

    const handleSelect = useCallback((placeholderId: string) => {
        suggestionCommandRef.current?.({ id: placeholderId });
    }, []);

    const handleDismiss = useCallback(() => {
        // Programmatically exit the Suggestion plugin — otherwise decorations
        // linger and `onExit` does not fire until the next transaction.
        if (editor) {
            exitSuggestion(editor.view, PlaceholderMentionPluginKey);
        }
        setSuggestionActive(false);
    }, [editor]);

    const wrapperClass =
        "w-full max-w-[720px] px-[12px] py-[10px] rounded-[8px] border-1 border-[var(--border-secondary)] bg-transparent placeholder:text-[var(--text-tertiary)] focus-within:border-[var(--accent)] focus-within:shadow-[0_0_0_1px_var(--accent)] transition-all" +
        (fill ? " flex-1 min-h-0 overflow-auto" : "") +
        (readOnly ? " opacity-70 cursor-not-allowed bg-[var(--bg-tertiary)]" : "");

    return (
        <div className={wrapperClass}>
            <EditorContent editor={editor} />
            <PlaceholderAutocomplete
                query={suggestionQuery}
                visible={suggestionActive}
                anchorRect={suggestionRect}
                onSelect={handleSelect}
                onDismiss={handleDismiss}
            />
        </div>
    );
}
