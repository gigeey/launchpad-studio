import type React from "react";
import { Paperclip } from "lucide-react";
import type { FormRequestPayload, FormAnswerMap, AsyncFormAnswerMeta, FormAnswerValue } from "../../types/form";
import { adjustBubbleColor } from "../../lib/colorUtils";
import { useIsDark, useUserPreferencesStore } from "../../stores/userPreferencesStore";

export interface FormAnswerBubbleProps {
  form: FormRequestPayload;
  answers: FormAnswerMap;
}

export function FormAnswerBubble({ form, answers }: FormAnswerBubbleProps) {
  const isDark = useIsDark();
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const bg = adjustBubbleColor(bubbleColor || "#1164A3", isDark);

  return (
    <div className="flex justify-end">
      <div
        className="max-w-[85%] @3xl:max-w-[75%] @5xl:max-w-[70%] px-[14px] py-[10px] text-[var(--text-user-message)] text-[14px] leading-[1.45] break-words"
        style={{ borderRadius: "16px 4px 16px 16px", backgroundColor: bg }}
      >
        <div className="font-semibold text-[13px] pb-[6px] border-b border-white/20 mb-[8px]">
          {form.title}
        </div>
        {form.intro && (
          <div className="text-[12px] opacity-80 mb-[8px]">{form.intro}</div>
        )}
        <div className="flex flex-col gap-[8px]">
          {form.fields.map((field) => {
            const answer = answers[field.id];
            if (!answer) return null;
            return (
              <div key={field.id}>
                <div className="text-[11px] font-medium opacity-70 uppercase tracking-wide">
                  {field.label}
                </div>
                <div className="text-[13px] mt-[2px]">
                  {answer.kind === "text" && (answer.value ?? "")}
                  {answer.kind === "selections" && (() => {
                    const labels = (answer.values ?? []).map((selId) => {
                      const opt = field.options?.find((o) => o.id === selId);
                      return opt?.label ?? selId;
                    });
                    return labels.join(", ");
                  })()}
                  {answer.kind === "files" && (
                    <div className="flex flex-wrap gap-[4px] mt-[2px]">
                      {(answer.attachment_ids ?? []).map((aid) => (
                        <span
                          key={aid}
                          className="inline-flex items-center gap-[4px] px-[8px] py-[2px] rounded-full bg-white/20 text-[12px]"
                        >
                          <Paperclip className="w-[10px] h-[10px]" />
                          {aid.slice(0, 8)}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function renderAsyncValue(val: unknown): React.ReactNode {
  if (val !== null && typeof val === "object" && "kind" in val) {
    const v = val as FormAnswerValue;
    if (v.kind === "text") return v.value ?? "";
    if (v.kind === "selections") return (v.values ?? []).join(", ");
    if (v.kind === "files") {
      return (
        <div className="flex flex-wrap gap-[4px] mt-[2px]">
          {(v.attachment_ids ?? []).map((aid) => (
            <span
              key={aid}
              className="inline-flex items-center gap-[4px] px-[8px] py-[2px] rounded-full bg-white/20 text-[12px]"
            >
              <Paperclip className="w-[10px] h-[10px]" />
              {aid.slice(0, 8)}
            </span>
          ))}
        </div>
      );
    }
  }
  if (typeof val === "string") return val;
  return JSON.stringify(val);
}

/** Renders one line of a self-rendering `form_answer` entry's `content`
 *  (see the backend's `form_answer_content` —
 *  crates/ao-engine-tools-core/src/form_events.rs). The backend hand-formats
 *  content as a `**title**` line (when the form had one) followed by
 *  `- label: answer` lines — this is a light, purpose-built parse of exactly
 *  that shape, not a general markdown renderer.
 */
function renderAnswerContentLine(line: string, key: number): React.ReactNode {
  const trimmed = line.trim();
  if (!trimmed) return null;
  const boldMatch = /^\*\*(.+)\*\*$/.exec(trimmed);
  if (boldMatch) {
    return (
      <div
        key={key}
        className="font-semibold text-[13px] pb-[6px] border-b border-white/20 mb-[8px]"
      >
        {boldMatch[1]}
      </div>
    );
  }
  return (
    <div key={key} className="text-[13px]">
      {trimmed.replace(/^-\s*/, "")}
    </div>
  );
}

/** Self-rendering `form_answer` bubble: renders the entry's own `content`
 *  directly, with no dependency on a `form_request` entry being loaded in
 *  the same window and no join against `pending_forms` — `content` alone
 *  already names the question(s) asked and the answer(s) given. This is the
 *  preferred renderer for any entry written post-fix (non-empty `content`);
 *  `AsyncFormAnswerBubble` below stays as the fallback for legacy entries
 *  whose `content` is still `""`.
 */
export function AsyncFormAnswerContentBubble({ content }: { content: string }) {
  const isDark = useIsDark();
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const bg = adjustBubbleColor(bubbleColor || "#1164A3", isDark);

  return (
    <div className="flex justify-end">
      <div
        className="max-w-[85%] @3xl:max-w-[75%] @5xl:max-w-[70%] px-[14px] py-[10px] text-[var(--text-user-message)] text-[14px] leading-[1.45] break-words"
        style={{ borderRadius: "16px 4px 16px 16px", backgroundColor: bg }}
        data-testid="form-answer-content-bubble"
      >
        <div className="flex flex-col gap-[6px]">
          {content.split("\n").map((line, i) => renderAnswerContentLine(line, i))}
        </div>
      </div>
    </div>
  );
}

export function AsyncFormAnswerBubble({ meta }: { meta: AsyncFormAnswerMeta }) {
  const isDark = useIsDark();
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const bg = adjustBubbleColor(bubbleColor || "#1164A3", isDark);
  const entries = Object.entries(meta.values);

  return (
    <div className="flex justify-end">
      <div
        className="max-w-[85%] @3xl:max-w-[75%] @5xl:max-w-[70%] px-[14px] py-[10px] text-[var(--text-user-message)] text-[14px] leading-[1.45] break-words"
        style={{ borderRadius: "16px 4px 16px 16px", backgroundColor: bg }}
        data-testid="async-form-answer-bubble"
      >
        <div className="font-semibold text-[13px] pb-[6px] border-b border-white/20 mb-[8px]">
          Form response
        </div>
        <div className="flex flex-col gap-[8px]">
          {entries.length === 0 ? (
            <span className="text-[13px] opacity-80">Submitted</span>
          ) : (
            entries.map(([key, val]) => (
              <div key={key}>
                <div className="text-[11px] font-medium opacity-70 uppercase tracking-wide">{key}</div>
                <div className="text-[13px] mt-[2px]">{renderAsyncValue(val)}</div>
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}
