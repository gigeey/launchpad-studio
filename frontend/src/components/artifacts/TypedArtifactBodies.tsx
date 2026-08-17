// The six typed renderers: each draws structured JSON
// for one `ArtifactKind`. None of these lock down a strict schema — the
// authoring tool only guarantees "payload is a JSON object" — so each is
// tolerant of missing/extra fields and falls back to `RawPayloadFallback`
// when the payload doesn't look like its expected shape at all.

import { asNumber, asString, firstArray, firstString, formatCell, isRecord } from "./payloadGuards";
import { RawPayloadFallback } from "./RawPayloadFallback";
import type { ArtifactBodyProps } from "./types";

const LABEL_STYLE = { color: "var(--text-primary)" } as const;
const MUTED_STYLE = { color: "var(--text-secondary)" } as const;
const BORDER_STYLE = { borderColor: "var(--border-primary)" } as const;

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

interface ListItemLike {
  title?: string;
  subtitle?: string;
  description?: string;
}

function toListItem(item: unknown): ListItemLike {
  if (typeof item === "string") return { title: item };
  if (isRecord(item)) {
    return {
      title: asString(item.title),
      subtitle: asString(item.subtitle),
      description: asString(item.description),
    };
  }
  return {};
}

export function ListArtifactBody({ artifact }: ArtifactBodyProps) {
  const payload = artifact.payload;
  const items = firstArray(payload, ["items", "list", "data", "entries"]);
  if (!items) {
    return (
      <div data-testid="artifact-body-list">
        <RawPayloadFallback payload={payload} />
      </div>
    );
  }
  return (
    <ul data-testid="artifact-body-list" className="flex flex-col gap-2">
      {items.map((raw, i) => {
        const item = toListItem(raw);
        return (
          <li key={i} className="rounded-[10px] border px-3 py-2" style={BORDER_STYLE}>
            {item.title && <div className="text-[13px] font-semibold" style={LABEL_STYLE}>{item.title}</div>}
            {item.subtitle && <div className="text-[12px]" style={MUTED_STYLE}>{item.subtitle}</div>}
            {item.description && (
              <div className="text-[12.5px] mt-1 leading-relaxed" style={LABEL_STYLE}>
                {item.description}
              </div>
            )}
          </li>
        );
      })}
    </ul>
  );
}

// ---------------------------------------------------------------------------
// cards
// ---------------------------------------------------------------------------

interface CardItemLike {
  title?: string;
  subtitle?: string;
  description?: string;
  image?: string;
}

// LLM-authored "cards" payloads name the same concepts differently across
// generations — a title might arrive as "name" or "heading", a description as
// "body" or "summary". Each field below tries its primary key first, then
// falls back through the synonyms other generations tend to use.
const CARD_TITLE_KEYS = ["title", "name", "heading", "label"];
const CARD_SUBTITLE_KEYS = ["subtitle", "tag", "category"];
const CARD_DESCRIPTION_KEYS = ["description", "text", "body", "summary"];
const CARD_IMAGE_KEYS = ["image", "img", "thumbnail", "url"];

function toCardItem(item: unknown): CardItemLike {
  if (!isRecord(item)) return {};
  return {
    title: firstString(item, CARD_TITLE_KEYS),
    subtitle: firstString(item, CARD_SUBTITLE_KEYS),
    description: firstString(item, CARD_DESCRIPTION_KEYS),
    image: firstString(item, CARD_IMAGE_KEYS),
  };
}

function isRenderableCard(item: CardItemLike): boolean {
  return Boolean(item.title || item.subtitle || item.description || item.image);
}

export function CardsArtifactBody({ artifact }: ArtifactBodyProps) {
  const payload = artifact.payload;
  // Accept a bare top-level array (no wrapper object at all) in addition to
  // the usual `{ items: [...] }` shape and its common synonyms.
  const rawItems = Array.isArray(payload) ? payload : firstArray(payload, ["items", "cards", "data", "entries"]);
  const items = rawItems ? rawItems.map(toCardItem) : null;
  // A container was found, but if not a single item carries any recognizable
  // field, showing empty boxes would be worse than the formatted fallback —
  // fall back only in that case, never just because the shape wasn't exactly
  // `{ items: [...] }`.
  if (!items || !items.some(isRenderableCard)) {
    return (
      <div data-testid="artifact-body-cards">
        <RawPayloadFallback payload={payload} />
      </div>
    );
  }
  return (
    <div
      data-testid="artifact-body-cards"
      className="grid gap-3"
      style={{ gridTemplateColumns: "repeat(auto-fill, minmax(180px, 1fr))" }}
    >
      {items.map((item, i) => (
        <div key={i} className="rounded-[10px] border p-3 flex flex-col gap-1" style={BORDER_STYLE}>
          {item.image && (
            <img src={item.image} alt={item.title ?? ""} className="w-full h-24 object-cover rounded-[8px]" />
          )}
          {item.title && <div className="text-[13px] font-semibold" style={LABEL_STYLE}>{item.title}</div>}
          {item.subtitle && <div className="text-[12px]" style={MUTED_STYLE}>{item.subtitle}</div>}
          {item.description && <div className="text-[12px] leading-relaxed" style={LABEL_STYLE}>{item.description}</div>}
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// table
// ---------------------------------------------------------------------------

export function TableArtifactBody({ artifact }: ArtifactBodyProps) {
  const payload = artifact.payload;
  const columnsRaw = firstArray(payload, ["columns", "headers"]);
  const columns = columnsRaw ? columnsRaw.map((c) => asString(c) ?? formatCell(c)) : null;
  const rows = firstArray(payload, ["rows", "data"]);
  if (!columns || !rows) {
    return (
      <div data-testid="artifact-body-table">
        <RawPayloadFallback payload={payload} />
      </div>
    );
  }
  return (
    <table data-testid="artifact-body-table" className="w-full text-[12.5px] border-collapse">
      <thead>
        <tr>
          {columns.map((col, i) => (
            <th
              key={i}
              className="text-left border-b px-2 py-1.5 font-semibold"
              style={{ ...BORDER_STYLE, ...LABEL_STYLE }}
            >
              {col}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((row, ri) => (
          <tr key={ri}>
            {columns.map((col, ci) => (
              <td key={ci} className="border-b px-2 py-1.5" style={{ ...BORDER_STYLE, ...LABEL_STYLE }}>
                {Array.isArray(row) ? formatCell(row[ci]) : isRecord(row) ? formatCell(row[col]) : ""}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

// ---------------------------------------------------------------------------
// board
// ---------------------------------------------------------------------------

interface BoardColumnLike {
  title?: string;
  items?: unknown[];
}

function toBoardColumn(col: unknown): BoardColumnLike {
  if (!isRecord(col)) return {};
  return {
    title: asString(col.title),
    items: Array.isArray(col.items) ? col.items : undefined,
  };
}

export function BoardArtifactBody({ artifact }: ArtifactBodyProps) {
  const payload = artifact.payload;
  const columns = firstArray(payload, ["columns", "lanes"]);
  if (!columns) {
    return (
      <div data-testid="artifact-body-board">
        <RawPayloadFallback payload={payload} />
      </div>
    );
  }
  return (
    <div data-testid="artifact-body-board" className="flex gap-3 h-full overflow-x-auto">
      {columns.map((raw, ci) => {
        const col = toBoardColumn(raw);
        return (
          <div
            key={ci}
            className="flex-shrink-0 w-[220px] rounded-[10px] border p-2 flex flex-col gap-2"
            style={BORDER_STYLE}
          >
            <div className="text-[12.5px] font-semibold px-1" style={LABEL_STYLE}>
              {col.title ?? `Column ${ci + 1}`}
            </div>
            {(col.items ?? []).map((raw2, ii) => {
              const item = isRecord(raw2) ? raw2 : {};
              return (
                <div key={ii} className="rounded-[8px] border px-2 py-1.5" style={BORDER_STYLE}>
                  <div className="text-[12px] font-medium" style={LABEL_STYLE}>
                    {asString(item.title) ?? formatCell(raw2)}
                  </div>
                  {asString(item.subtitle) && (
                    <div className="text-[11px]" style={MUTED_STYLE}>
                      {asString(item.subtitle)}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// metric
// ---------------------------------------------------------------------------

interface MetricLike {
  label?: string;
  value?: unknown;
  delta?: unknown;
}

function toMetric(raw: unknown): MetricLike | null {
  if (!isRecord(raw)) return null;
  return { label: asString(raw.label), value: raw.value, delta: raw.delta };
}

export function MetricArtifactBody({ artifact }: ArtifactBodyProps) {
  const payload = artifact.payload;
  let metrics: MetricLike[] | null = null;
  const metricsRaw = firstArray(payload, ["metrics", "data"]);
  if (metricsRaw) {
    metrics = metricsRaw.map(toMetric).filter((m): m is MetricLike => m !== null);
  } else if (isRecord(payload) && "value" in payload) {
    // Single flat { label, value, delta } metric.
    const single = toMetric(payload);
    metrics = single ? [single] : null;
  }
  if (!metrics || metrics.length === 0) {
    return (
      <div data-testid="artifact-body-metric">
        <RawPayloadFallback payload={payload} />
      </div>
    );
  }
  return (
    <div
      data-testid="artifact-body-metric"
      className="grid gap-3"
      style={{ gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))" }}
    >
      {metrics.map((m, i) => (
        <div key={i} className="rounded-[10px] border p-3 flex flex-col gap-1" style={BORDER_STYLE}>
          {m.label && <div className="text-[12px]" style={MUTED_STYLE}>{m.label}</div>}
          <div className="text-[22px] font-semibold" style={LABEL_STYLE}>
            {formatCell(m.value)}
          </div>
          {m.delta !== undefined && (
            <div className="text-[11px]" style={MUTED_STYLE}>
              {formatCell(m.delta)}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// chart
// ---------------------------------------------------------------------------

interface ChartSeriesLike {
  name?: string;
  values: number[];
}

const CHART_COLORS = ["#6366f1", "#22c55e", "#f59e0b", "#ec4899", "#0ea5e9"];

export function ChartArtifactBody({ artifact }: ArtifactBodyProps) {
  const payload = artifact.payload;
  const labels =
    isRecord(payload) && Array.isArray(payload.labels) ? payload.labels.map((l) => asString(l) ?? formatCell(l)) : null;
  const rawSeries = isRecord(payload) && Array.isArray(payload.series) ? payload.series : null;
  const series: ChartSeriesLike[] | null = rawSeries
    ? rawSeries
        .map((s): ChartSeriesLike | null =>
          isRecord(s) && Array.isArray(s.values)
            ? { name: asString(s.name), values: s.values.map((v) => asNumber(v) ?? 0) }
            : null,
        )
        .filter((s): s is ChartSeriesLike => s !== null)
    : null;

  if (!labels || !series || series.length === 0) {
    return (
      <div data-testid="artifact-body-chart">
        <RawPayloadFallback payload={payload} />
      </div>
    );
  }

  const max = Math.max(1, ...series.flatMap((s) => s.values));
  const chartHeight = 160;
  const groupWidth = 48;
  const barWidth = Math.max(4, Math.floor((groupWidth - 8) / series.length));
  const width = Math.max(labels.length * groupWidth, groupWidth);

  return (
    <div data-testid="artifact-body-chart" className="flex flex-col gap-2">
      <svg
        role="img"
        aria-label={artifact.title}
        width="100%"
        height={chartHeight + 24}
        viewBox={`0 0 ${width} ${chartHeight + 24}`}
        preserveAspectRatio="xMinYMid meet"
      >
        {labels.map((label, li) => (
          <g key={li} transform={`translate(${li * groupWidth}, 0)`}>
            {series.map((s, si) => {
              const value = s.values[li] ?? 0;
              const barHeight = Math.round((value / max) * chartHeight);
              return (
                <rect
                  key={si}
                  x={4 + si * barWidth}
                  y={chartHeight - barHeight}
                  width={Math.max(1, barWidth - 2)}
                  height={barHeight}
                  fill={CHART_COLORS[si % CHART_COLORS.length]}
                />
              );
            })}
            <text
              x={groupWidth / 2}
              y={chartHeight + 16}
              textAnchor="middle"
              fontSize="9"
              fill="var(--text-secondary)"
            >
              {label}
            </text>
          </g>
        ))}
      </svg>
      {series.some((s) => s.name) && (
        <div className="flex gap-3 flex-wrap px-1">
          {series.map((s, i) => (
            <div key={i} className="flex items-center gap-1.5 text-[11px]" style={MUTED_STYLE}>
              <span
                className="w-2.5 h-2.5 rounded-full inline-block"
                style={{ backgroundColor: CHART_COLORS[i % CHART_COLORS.length] }}
              />
              {s.name ?? `Series ${i + 1}`}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
