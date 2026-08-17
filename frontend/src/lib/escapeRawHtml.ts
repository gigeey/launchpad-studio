/**
 * Escape `<` and `>` to `&lt;` / `&gt;` in text that is **outside** of:
 *   - fenced code blocks (``` … ```)
 *   - inline code spans (`…`)
 *
 * This prevents unknown XML-like tags (e.g. `<write_skill>`, `<delegation>`)
 * from being silently swallowed by react-markdown / rehype.
 *
 * Well-formatted content (tags inside backticks or code blocks) passes through
 * unchanged. The function acts as a safety net for unescaped tags in prose.
 */
export function escapeRawHtmlOutsideCode(text: string): string {
  // Split on fenced code blocks and inline code spans, preserving delimiters.
  // Fenced blocks: ```...``` (possibly with language tag and content)
  // Inline code:   `...` (no nesting)
  const parts = text.split(/(```[\s\S]*?```|`[^`]+`)/g);

  return parts
    .map((part, i) => {
      // Odd indices are code spans/blocks — leave untouched
      if (i % 2 === 1) return part;
      // Even indices are prose — escape angle brackets
      return part.replace(/</g, "&lt;").replace(/>/g, "&gt;");
    })
    .join("");
}
