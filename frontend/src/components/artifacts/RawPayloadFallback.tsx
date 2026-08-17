import { formatCell, isRecord } from "./payloadGuards";

const KEY_STYLE = { color: "var(--text-secondary)" } as const;
const VALUE_STYLE = { color: "var(--text-primary)" } as const;
const BORDER_STYLE = { borderColor: "var(--border-primary)" } as const;

// Long values (a nested object dumped through `formatCell`, a giant string
// field, etc.) get an ellipsis rather than pushing the whole card into a
// scroll-fest — this is a display truncation only, the underlying payload is
// untouched and still available via the artifact's own "copy raw" control.
const MAX_VALUE_LENGTH = 500;

function truncate(text: string): string {
  return text.length > MAX_VALUE_LENGTH ? `${text.slice(0, MAX_VALUE_LENGTH)}…` : text;
}

/** Last-resort display for a typed payload that doesn't match the shape its
 *  renderer expected — a formatted key/value view rather than a naked JSON
 *  dump. Distinct from `UnsupportedArtifactBody`: that one handles an
 *  unrecognized `kind`; this one handles a recognized `kind` whose payload
 *  didn't match any shape the renderer knows how to draw as cards/list/table/
 *  etc. Both share the same goal — never throw, never blank-screen, and never
 *  leave the person staring at a raw code blob. */
export function RawPayloadFallback({ payload }: { payload: unknown }) {
  if (isRecord(payload)) {
    const keys = Object.keys(payload);
    if (keys.length === 0) {
      return (
        <div data-testid="artifact-raw-fallback" className="text-[12.5px]" style={VALUE_STYLE}>
          (empty)
        </div>
      );
    }
    return (
      <div data-testid="artifact-raw-fallback" className="flex flex-col gap-2">
        {keys.map((key) => (
          <div key={key} className="rounded-[10px] border px-3 py-2" style={BORDER_STYLE}>
            <div className="text-[11px] uppercase tracking-wide" style={KEY_STYLE}>
              {key}
            </div>
            <div
              className="text-[12.5px] mt-0.5 leading-relaxed whitespace-pre-wrap break-words"
              style={VALUE_STYLE}
            >
              {truncate(formatCell(payload[key]))}
            </div>
          </div>
        ))}
      </div>
    );
  }

  if (Array.isArray(payload)) {
    if (payload.length === 0) {
      return (
        <div data-testid="artifact-raw-fallback" className="text-[12.5px]" style={VALUE_STYLE}>
          (empty)
        </div>
      );
    }
    return (
      <ul data-testid="artifact-raw-fallback" className="flex flex-col gap-2">
        {payload.map((item, i) => (
          <li
            key={i}
            className="rounded-[10px] border px-3 py-2 text-[12.5px] leading-relaxed whitespace-pre-wrap break-words"
            style={{ ...BORDER_STYLE, ...VALUE_STYLE }}
          >
            {truncate(formatCell(item))}
          </li>
        ))}
      </ul>
    );
  }

  return (
    <div
      data-testid="artifact-raw-fallback"
      className="text-[12.5px] leading-relaxed whitespace-pre-wrap break-words"
      style={VALUE_STYLE}
    >
      {truncate(formatCell(payload))}
    </div>
  );
}
