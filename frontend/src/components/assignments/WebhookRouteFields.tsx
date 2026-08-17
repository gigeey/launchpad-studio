import { useCallback, useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Copy, Eye, EyeOff, Loader2, Plus, RefreshCw, X } from "lucide-react";
import { BASE_URL, getWebhookRouteSecretStatus, setWebhookRouteSecret, testWebhookRoute } from "../../lib/api";
import type { AssignmentTrigger, TestWebhookRouteResponse, WebhookFieldFilter, WebhookFilter } from "../../lib/api";

/** One `WebhookFilter` field-level test edited as a plain row — the "field /
 *  op / value" shape the editor exposes. Multiple rows combine with an
 *  implicit AND (`WebhookFilter::All`) on submit; the backend's `any`/`not`/
 *  nested combinators exist but aren't row-editable here (see
 *  `rawFilterOverride` on [`WebhookRouteFieldsValue`]). */
export interface FilterRowState {
  id: string;
  field: string;
  op: WebhookFieldFilter["op"];
  /** Used by `equals` / `not_equals` / `contains`. Parsed as JSON when it
   *  parses (so `42`/`true` round-trip as numbers/booleans); falls back to
   *  the literal string otherwise. */
  value: string;
  /** Used by `in` — comma-separated, each entry parsed the same way as `value`. */
  valuesCsv: string;
  /** Used by `in_file`. */
  path: string;
  /** Used by `regex`. */
  pattern: string;
}

let filterRowIdCounter = 0;
function makeRowId(): string {
  filterRowIdCounter += 1;
  return `filter-row-${filterRowIdCounter}`;
}

export function emptyFilterRow(): FilterRowState {
  return { id: makeRowId(), field: "", op: "equals", value: "", valuesCsv: "", path: "", pattern: "" };
}

/** Everything the editor modal needs to build a `Webhook` trigger, kept as
 *  one bundled value (rather than a long list of individual props) because
 *  there are simply more moving parts here: route name, secret, events,
 *  filter rows, and template. */
export interface WebhookRouteFieldsValue {
  routeName: string;
  /** The secret value known to this client this session — freshly typed or
   *  generated. Empty when editing a route whose secret was set in an
   *  earlier session: the store is write-only, so it can never be
   *  re-fetched and redisplayed here. */
  secret: string;
  events: string[];
  filterRows: FilterRowState[];
  /** A filter tree from an existing assignment that doesn't reduce to a flat
   *  AND-list of field filters (uses `any`/`not`, or nesting) — preserved
   *  verbatim on save so opening such a route in this editor never silently
   *  discards structure the row UI can't represent. Cleared the moment the
   *  user adds/edits/removes a row, since at that point they've taken
   *  manual control of `filterRows` instead. */
  rawFilterOverride: WebhookFilter | null;
  /** Legacy `token` shared-secret, carried through unchanged. No control in
   *  this component edits it (superseded by `secret_ref`-based HMAC), but an
   *  existing assignment's token must not be silently dropped on save. */
  legacyToken: string | null;
}

export const DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE: WebhookRouteFieldsValue = {
  routeName: "",
  secret: "",
  events: [],
  filterRows: [],
  rawFilterOverride: null,
  legacyToken: null,
};

function isFieldFilter(f: WebhookFilter): f is WebhookFieldFilter {
  return "field" in f;
}

function stringifyFilterValue(v: unknown): string {
  return typeof v === "string" ? v : JSON.stringify(v);
}

function parseFilterValue(text: string): unknown {
  const trimmed = text.trim();
  if (!trimmed) return "";
  try {
    return JSON.parse(trimmed);
  } catch {
    return trimmed;
  }
}

function fieldFilterToRow(f: WebhookFieldFilter): FilterRowState {
  const base = { id: makeRowId(), field: f.field, value: "", valuesCsv: "", path: "", pattern: "" };
  switch (f.op) {
    case "equals":
    case "not_equals":
    case "contains":
      return { ...base, op: f.op, value: stringifyFilterValue(f.value) };
    case "in":
      return { ...base, op: "in", valuesCsv: f.values.map(stringifyFilterValue).join(", ") };
    case "in_file":
      return { ...base, op: "in_file", path: f.path };
    case "regex":
      return { ...base, op: "regex", pattern: f.pattern };
    case "exists":
    case "missing":
      return { ...base, op: f.op };
  }
}

/** Inverse of [`buildWebhookFilterFromRows`], for prefilling an existing
 *  assignment. Only a single field filter or a flat `all` list of field
 *  filters round-trips to rows; anything else (any/not/nested) comes back as
 *  `rawOverride` instead, preserved verbatim. */
export function filterRowsFromWebhookFilter(filter: WebhookFilter | null | undefined): {
  rows: FilterRowState[];
  rawOverride: WebhookFilter | null;
} {
  if (!filter) return { rows: [], rawOverride: null };
  if (isFieldFilter(filter)) return { rows: [fieldFilterToRow(filter)], rawOverride: null };
  if ("all" in filter) {
    const rows: FilterRowState[] = [];
    for (const sub of filter.all) {
      if (!isFieldFilter(sub)) return { rows: [], rawOverride: filter };
      rows.push(fieldFilterToRow(sub));
    }
    return { rows, rawOverride: null };
  }
  return { rows: [], rawOverride: filter };
}

function filterRowToField(row: FilterRowState): WebhookFieldFilter | null {
  const field = row.field.trim();
  if (!field) return null;
  switch (row.op) {
    case "equals":
      return { field, op: "equals", value: parseFilterValue(row.value) };
    case "not_equals":
      return { field, op: "not_equals", value: parseFilterValue(row.value) };
    case "contains":
      return { field, op: "contains", value: parseFilterValue(row.value) };
    case "in":
      return {
        field,
        op: "in",
        values: row.valuesCsv
          .split(",")
          .map((v) => v.trim())
          .filter((v) => v.length > 0)
          .map(parseFilterValue),
      };
    case "in_file":
      return { field, op: "in_file", path: row.path.trim() };
    case "regex":
      return { field, op: "regex", pattern: row.pattern.trim() };
    case "exists":
      return { field, op: "exists" };
    case "missing":
      return { field, op: "missing" };
  }
}

/** Combines filter rows into the wire `WebhookFilter` tree: `null` when
 *  there are none, the bare field filter when there's exactly one, otherwise
 *  an `all` (AND) list. Rows with a blank `field` are dropped. */
export function buildWebhookFilterFromRows(rows: FilterRowState[]): WebhookFilter | null {
  const fields = rows.map(filterRowToField).filter((f): f is WebhookFieldFilter => f !== null);
  if (fields.length === 0) return null;
  if (fields.length === 1) return fields[0];
  return { all: fields };
}

/** Builds the `Webhook` trigger this component's value maps to. `route_name`
 *  and `secret_ref` are always set equal — see the doc comment on
 *  `AssignmentTrigger`'s `Webhook.secret_ref` field for why that's a real
 *  backend-enforced equivalence, not just a UI convention.
 *
 *  `promptTemplate` is the editor's single top Instruction field, not a
 *  control owned by this component — the caller passes it through so the
 *  same text that's rendered on-screen is what the backend interpolates
 *  `{dot.path}`/`{__raw__}` placeholders against at fire time. */
export function buildWebhookTrigger(value: WebhookRouteFieldsValue, promptTemplate: string): AssignmentTrigger {
  const routeName = value.routeName.trim();
  return {
    type: "Webhook",
    token: value.legacyToken,
    route_name: routeName ? routeName : null,
    secret_ref: routeName ? routeName : null,
    events: value.events,
    filters: value.rawFilterOverride ?? buildWebhookFilterFromRows(value.filterRows),
    prompt_template: promptTemplate.trim() ? promptTemplate : null,
    deliver: { type: "agent" },
  };
}

/** Prefills this component's value from an existing assignment's `Webhook`
 *  trigger (edit mode). `secret` always starts empty — see the field's doc
 *  comment. The trigger's own `prompt_template` is handled by the caller
 *  (it seeds the shared top Instruction field, not anything owned here). */
export function webhookFieldsFromTrigger(trigger: Extract<AssignmentTrigger, { type: "Webhook" }>): WebhookRouteFieldsValue {
  const { rows, rawOverride } = filterRowsFromWebhookFilter(trigger.filters);
  return {
    routeName: trigger.route_name ?? "",
    secret: "",
    events: trigger.events ?? [],
    filterRows: rows,
    rawFilterOverride: rawOverride,
    legacyToken: trigger.token ?? null,
  };
}

/** If `value.secret` is non-empty, persists it to the route's secret store
 *  under `value.routeName`. Called by the editor modal right after a
 *  successful create/save — deliberately not wired to any control inside
 *  this component, since the secret should be saved atomically with the
 *  rest of the assignment, never independently. No-op when there's nothing
 *  to save. */
export async function saveWebhookRouteSecretIfPresent(value: WebhookRouteFieldsValue): Promise<void> {
  const routeName = value.routeName.trim();
  const secret = value.secret.trim();
  if (!routeName || !secret) return;
  await setWebhookRouteSecret(routeName, secret);
}

const DEFAULT_TEST_EVENT_TYPE = "example.event";
const DEFAULT_TEST_PAYLOAD_TEXT = JSON.stringify(
  {
    event: { type: "example.event" },
    resource: { name: "demo" },
  },
  null,
  2,
);

const FILTER_OP_OPTIONS: { value: FilterRowState["op"]; label: string }[] = [
  { value: "equals", label: "equals" },
  { value: "not_equals", label: "not equals" },
  { value: "contains", label: "contains" },
  { value: "in", label: "in (comma-separated)" },
  { value: "in_file", label: "in file (one value per line)" },
  { value: "regex", label: "matches regex" },
  { value: "exists", label: "field exists" },
  { value: "missing", label: "field is missing" },
];

const inputClass =
  "w-full bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] px-[10px] py-[8px] text-[14px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-secondary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors";
const smallInputClass = `${inputClass} text-[13px] px-[8px] py-[6px]`;
const errorClass = "text-[11px] text-[var(--error)]";
const hintClass = "text-[12px] text-[var(--modal-text-secondary)]";
const labelClass = "text-[11px] uppercase tracking-wide text-[var(--modal-text-secondary)]";
const iconButtonClass =
  "flex items-center justify-center gap-1.5 px-[10px] h-[36px] rounded-[8px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] font-semibold text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed";

interface WebhookRouteFieldsProps {
  value: WebhookRouteFieldsValue;
  onChange: (value: WebhookRouteFieldsValue) => void;
  /** The editor's single top Instruction field, rendered against the
   *  inbound payload as this route's `prompt_template` at fire time — see
   *  `buildWebhookTrigger`. Used here only so "Send test webhook" dry-runs
   *  against exactly what will actually fire. */
  promptTemplate: string;
}

/** Fields for the `Webhook` trigger tab — the push-side counterpart to the
 *  Watch tab's cadence-only field: route name, secret (generate/reveal/copy),
 *  the route's payload URL (copy), an `events` allowlist, declarative
 *  filters + the `{__raw__}` raw-payload escape hatch demoted under an
 *  "Advanced (Expert)" disclosure, and a "Send test webhook" dry run against
 *  the shared Instruction text. */
export function WebhookRouteFields({ value, onChange, promptTemplate }: WebhookRouteFieldsProps) {
  const [eventInput, setEventInput] = useState("");
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [secretRevealed, setSecretRevealed] = useState(false);
  const [secretStatus, setSecretStatus] = useState<{ loading: boolean; configured: boolean }>({
    loading: false,
    configured: false,
  });
  const [copied, setCopied] = useState<string | null>(null);

  const [testEventType, setTestEventType] = useState(DEFAULT_TEST_EVENT_TYPE);
  const [testPayloadText, setTestPayloadText] = useState(DEFAULT_TEST_PAYLOAD_TEXT);
  const [testResult, setTestResult] = useState<TestWebhookRouteResponse | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);

  const patch = (partial: Partial<WebhookRouteFieldsValue>) => onChange({ ...value, ...partial });

  const refreshSecretStatus = useCallback(async (routeName: string) => {
    const trimmed = routeName.trim();
    if (!trimmed) {
      setSecretStatus({ loading: false, configured: false });
      return;
    }
    setSecretStatus((s) => ({ ...s, loading: true }));
    try {
      const status = await getWebhookRouteSecretStatus(trimmed);
      setSecretStatus({ loading: false, configured: status.configured });
    } catch {
      setSecretStatus({ loading: false, configured: false });
    }
  }, []);

  // Mount-time check only (covers edit mode, where routeName is already
  // populated) — further checks happen on the route-name field's blur
  // rather than on every keystroke, to avoid a request per character typed.
  useEffect(() => {
    void refreshSecretStatus(value.routeName);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function copy(label: string, text: string) {
    if (!text) return;
    navigator.clipboard?.writeText(text).then(
      () => {
        setCopied(label);
        window.setTimeout(() => setCopied((c) => (c === label ? null : c)), 1500);
      },
      () => {},
    );
  }

  function generateSecret() {
    const bytes = new Uint8Array(32);
    (globalThis.crypto ?? window.crypto).getRandomValues(bytes);
    const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    patch({ secret: hex });
    setSecretRevealed(true);
  }

  function addEvent() {
    const v = eventInput.trim();
    if (!v || value.events.includes(v)) {
      setEventInput("");
      return;
    }
    patch({ events: [...value.events, v] });
    setEventInput("");
  }

  function removeEvent(ev: string) {
    patch({ events: value.events.filter((e) => e !== ev) });
  }

  function updateRow(id: string, partial: Partial<FilterRowState>) {
    patch({ filterRows: value.filterRows.map((r) => (r.id === id ? { ...r, ...partial } : r)), rawFilterOverride: null });
  }

  function addRow() {
    patch({ filterRows: [...value.filterRows, emptyFilterRow()], rawFilterOverride: null });
  }

  function removeRow(id: string) {
    patch({ filterRows: value.filterRows.filter((r) => r.id !== id), rawFilterOverride: null });
  }

  async function handleSendTestWebhook() {
    let payload: unknown;
    try {
      payload = JSON.parse(testPayloadText);
    } catch {
      setTestError("Sample payload must be valid JSON.");
      setTestResult(null);
      return;
    }
    setTesting(true);
    setTestError(null);
    setTestResult(null);
    try {
      const filters = value.rawFilterOverride ?? buildWebhookFilterFromRows(value.filterRows);
      const result = await testWebhookRoute({
        events: value.events,
        filters,
        prompt_template: promptTemplate.trim() ? promptTemplate : null,
        deliver: { type: "agent" },
        event_type: testEventType.trim() ? testEventType.trim() : null,
        payload,
      });
      setTestResult(result);
    } catch (err) {
      setTestError(err instanceof Error ? err.message : "Test failed.");
    } finally {
      setTesting(false);
    }
  }

  const routeName = value.routeName.trim();
  const payloadUrl = routeName ? `${BASE_URL}/webhooks/${encodeURIComponent(routeName)}` : null;
  const secretKnownClientSide = value.secret.length > 0;

  return (
    <div className="flex flex-col gap-[14px]">
      <div className="flex flex-col gap-[6px]">
        <label className={labelClass}>Route name</label>
        <input
          value={value.routeName}
          onChange={(e) => patch({ routeName: e.target.value })}
          onBlur={() => void refreshSecretStatus(value.routeName)}
          data-testid="assignment-editor-webhook-route-name-input"
          placeholder="e.g. deploy-events"
          className={`${inputClass} font-mono`}
        />
        {payloadUrl ? (
          <div className="flex items-center gap-2">
            <code className="flex-1 text-[12px] text-[var(--modal-text-secondary)] truncate" data-testid="assignment-editor-webhook-payload-url">
              {payloadUrl}
            </code>
            <button
              type="button"
              onClick={() => copy("url", payloadUrl)}
              data-testid="assignment-editor-webhook-copy-url"
              className={iconButtonClass}
            >
              <Copy size={13} />
              {copied === "url" ? "Copied" : "Copy URL"}
            </button>
          </div>
        ) : (
          <p className={hintClass}>Set a route name to get this route's payload URL (paste it into your webhook provider's settings).</p>
        )}
      </div>

      <div className="flex flex-col gap-[6px]">
        <label className={labelClass}>Secret</label>
        {secretKnownClientSide ? (
          <div className="flex items-center gap-2">
            <input
              type={secretRevealed ? "text" : "password"}
              value={value.secret}
              onChange={(e) => patch({ secret: e.target.value })}
              data-testid="assignment-editor-webhook-secret-input"
              placeholder="Shared HMAC signing secret"
              className={`flex-1 ${inputClass} font-mono`}
            />
            <button
              type="button"
              onClick={() => setSecretRevealed((r) => !r)}
              data-testid="assignment-editor-webhook-secret-reveal"
              className={iconButtonClass}
              aria-label={secretRevealed ? "Hide secret" : "Reveal secret"}
            >
              {secretRevealed ? <EyeOff size={13} /> : <Eye size={13} />}
            </button>
            <button
              type="button"
              onClick={() => copy("secret", value.secret)}
              data-testid="assignment-editor-webhook-secret-copy"
              className={iconButtonClass}
            >
              <Copy size={13} />
              {copied === "secret" ? "Copied" : "Copy"}
            </button>
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <div className="flex-1 text-[13px] text-[var(--modal-text-secondary)]" data-testid="assignment-editor-webhook-secret-status">
              {secretStatus.loading
                ? "Checking…"
                : secretStatus.configured
                  ? "Secret is configured (write-only — regenerate to see/copy a new one)."
                  : "No secret set — every request to this route will be rejected."}
            </div>
            <button
              type="button"
              onClick={generateSecret}
              data-testid="assignment-editor-webhook-secret-generate"
              className={iconButtonClass}
            >
              <RefreshCw size={13} />
              {secretStatus.configured ? "Regenerate" : "Generate"}
            </button>
          </div>
        )}
        <p className={hintClass}>Saved when you click Create/Save below. Paste this into your webhook provider's secret / signing field.</p>
      </div>

      <div className="flex flex-col gap-[6px]">
        <label className={labelClass}>Events</label>
        <div className="flex gap-2">
          <input
            value={eventInput}
            onChange={(e) => setEventInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addEvent();
              }
            }}
            data-testid="assignment-editor-webhook-event-input"
            placeholder="e.g. deploy.completed"
            className={`flex-1 ${inputClass} font-mono`}
          />
          <button type="button" onClick={addEvent} data-testid="assignment-editor-webhook-event-add" className={iconButtonClass}>
            <Plus size={13} />
            Add
          </button>
        </div>
        {value.events.length > 0 ? (
          <div className="flex flex-wrap gap-[6px]">
            {value.events.map((ev) => (
              <span
                key={ev}
                data-testid={`assignment-editor-webhook-event-chip-${ev}`}
                className="inline-flex items-center gap-1 px-[8px] py-[4px] rounded-full bg-[var(--modal-bg-hover)] text-[12px] font-mono text-[var(--modal-text-primary)]"
              >
                {ev}
                <button type="button" onClick={() => removeEvent(ev)} aria-label={`Remove ${ev}`} className="cursor-pointer">
                  <X size={11} />
                </button>
              </span>
            ))}
          </div>
        ) : (
          <p className={hintClass}>Empty allows every event type through to filters below.</p>
        )}
      </div>

      <div className="flex flex-col gap-[8px] border-t border-[var(--modal-border-primary)] pt-[12px]">
        <button
          type="button"
          onClick={() => setAdvancedOpen((o) => !o)}
          data-testid="assignment-editor-webhook-advanced-toggle"
          className="flex items-center gap-1.5 text-[13px] font-bold text-[var(--modal-text-primary)] cursor-pointer self-start"
        >
          {advancedOpen ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          Advanced (Expert)
        </button>

        {advancedOpen && (
          <div className="flex flex-col gap-[12px] pl-[4px]">
            <div className="flex flex-col gap-[6px]">
              <label className={labelClass}>Filters</label>
              <p className={hintClass}>
                Declarative, zero-token relevance rules evaluated against the raw payload before anything else — combined with
                AND. Leave empty for an unfiltered route (every event matching the allowlist above is delivered as-is; combine
                with <code>{"{__raw__}"}</code> in the prompt template to hand the agent the full raw payload).
              </p>
              {value.rawFilterOverride && (
                <p className={errorClass}>
                  This route's filters use a structure (any/not/nesting) not editable as rows here — it will be preserved as-is
                  unless you add a row below, which replaces it.
                </p>
              )}
              <div className="flex flex-col gap-[8px]">
                {value.filterRows.map((row) => (
                  <div key={row.id} className="flex flex-wrap items-center gap-2" data-testid="assignment-editor-webhook-filter-row">
                    <input
                      value={row.field}
                      onChange={(e) => updateRow(row.id, { field: e.target.value })}
                      placeholder="field, e.g. action"
                      data-testid="assignment-editor-webhook-filter-field"
                      className={`${smallInputClass} font-mono w-[160px]`}
                    />
                    <select
                      value={row.op}
                      onChange={(e) => updateRow(row.id, { op: e.target.value as FilterRowState["op"] })}
                      data-testid="assignment-editor-webhook-filter-op"
                      className={smallInputClass}
                    >
                      {FILTER_OP_OPTIONS.map((opt) => (
                        <option key={opt.value} value={opt.value}>
                          {opt.label}
                        </option>
                      ))}
                    </select>
                    {(row.op === "equals" || row.op === "not_equals" || row.op === "contains") && (
                      <input
                        value={row.value}
                        onChange={(e) => updateRow(row.id, { value: e.target.value })}
                        placeholder="value"
                        data-testid="assignment-editor-webhook-filter-value"
                        className={`${smallInputClass} font-mono flex-1 min-w-[120px]`}
                      />
                    )}
                    {row.op === "in" && (
                      <input
                        value={row.valuesCsv}
                        onChange={(e) => updateRow(row.id, { valuesCsv: e.target.value })}
                        placeholder="opened, reopened"
                        data-testid="assignment-editor-webhook-filter-values"
                        className={`${smallInputClass} font-mono flex-1 min-w-[120px]`}
                      />
                    )}
                    {row.op === "in_file" && (
                      <input
                        value={row.path}
                        onChange={(e) => updateRow(row.id, { path: e.target.value })}
                        placeholder="/path/to/allowlist.txt"
                        data-testid="assignment-editor-webhook-filter-path"
                        className={`${smallInputClass} font-mono flex-1 min-w-[120px]`}
                      />
                    )}
                    {row.op === "regex" && (
                      <input
                        value={row.pattern}
                        onChange={(e) => updateRow(row.id, { pattern: e.target.value })}
                        placeholder="(?i)^fix"
                        data-testid="assignment-editor-webhook-filter-pattern"
                        className={`${smallInputClass} font-mono flex-1 min-w-[120px]`}
                      />
                    )}
                    <button
                      type="button"
                      onClick={() => removeRow(row.id)}
                      aria-label="Remove filter"
                      data-testid="assignment-editor-webhook-filter-remove"
                      className="cursor-pointer text-[var(--modal-text-secondary)] hover:text-[var(--error)]"
                    >
                      <X size={14} />
                    </button>
                  </div>
                ))}
                <button
                  type="button"
                  onClick={addRow}
                  data-testid="assignment-editor-webhook-filter-add"
                  className={`${iconButtonClass} self-start`}
                >
                  <Plus size={13} />
                  Add filter
                </button>
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="flex flex-col gap-[8px] border-t border-[var(--modal-border-primary)] pt-[12px]">
        <label className={labelClass}>Send test webhook</label>
        <p className={hintClass}>
          Dry-runs your events/filters/template against a sample payload — no agent is started. This does not verify your
          signing secret; send a real event to confirm that.
        </p>
        <div className="flex gap-2">
          <div className="flex flex-col gap-[4px] w-[180px]">
            <label className={labelClass}>Event type</label>
            <input
              value={testEventType}
              onChange={(e) => setTestEventType(e.target.value)}
              data-testid="assignment-editor-webhook-test-event-type"
              className={`${smallInputClass} font-mono`}
            />
          </div>
          <div className="flex flex-col gap-[4px] flex-1">
            <label className={labelClass}>Sample payload (JSON)</label>
            <textarea
              value={testPayloadText}
              onChange={(e) => setTestPayloadText(e.target.value)}
              data-testid="assignment-editor-webhook-test-payload"
              rows={4}
              className={`${smallInputClass} font-mono resize-y`}
            />
          </div>
        </div>
        <button
          type="button"
          onClick={() => void handleSendTestWebhook()}
          disabled={testing}
          data-testid="assignment-editor-webhook-test-send"
          className={`${iconButtonClass} self-start`}
        >
          {testing && <Loader2 size={13} className="animate-spin" />}
          Send test webhook
        </button>
        {testError && (
          <span className={errorClass} data-testid="assignment-editor-webhook-test-error">
            {testError}
          </span>
        )}
        {testResult && (
          <div
            className="flex flex-col gap-[6px] p-[10px] rounded-[8px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)]"
            data-testid="assignment-editor-webhook-test-result"
          >
            <span className="text-[13px] font-semibold text-[var(--modal-text-primary)]">
              {testResult.matched ? "Matched" : "Filtered out"}
              {testResult.matched && ` — ${testResult.would_start_agent ? "would start the agent" : "would deliver without starting the agent"}`}
            </span>
            {testResult.rendered_instruction != null && (
              <pre className="text-[12px] font-mono whitespace-pre-wrap text-[var(--modal-text-secondary)]">
                {testResult.rendered_instruction}
              </pre>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
