import { useEffect, useState } from "react";
import { ArrowRight, CheckCircle2, Loader2 } from "lucide-react";
import * as api from "../../lib/api";
import type { InputField, PhaseStatus } from "../../types/workflow";

interface InputPhaseFormProps {
  taskId: string;
  phaseId: string;
  phaseName: string;
  fields: InputField[];
  phaseStatus?: PhaseStatus;
}

export function InputPhaseForm({ taskId, phaseId, phaseName, fields, phaseStatus }: InputPhaseFormProps) {
  const [values, setValues] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {};
    for (const f of fields) {
      init[f.name] = "";
    }
    return init;
  });
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [submittedValues, setSubmittedValues] = useState<Record<string, string> | null>(null);
  const isCompleted = phaseStatus === "completed";

  // Load submitted values when phase is completed
  useEffect(() => {
    if (!isCompleted) return;
    api.getTaskOutput(taskId, "inputs.yaml").then((content) => {
      const parsed: Record<string, string> = {};
      for (const line of content.split("\n")) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith("#")) continue;
        const idx = trimmed.indexOf(":");
        if (idx > 0) {
          parsed[trimmed.slice(0, idx).trim()] = trimmed.slice(idx + 1).trim().replace(/^["']|["']$/g, "");
        }
      }
      setSubmittedValues(parsed);
    }).catch(() => {});
  }, [isCompleted, taskId]);

  const hasAllRequired = fields.every(
    (f) => f.required === false || (values[f.name]?.trim() ?? "") !== "",
  );

  const handleChange = (name: string, value: string) => {
    setValues((prev) => ({ ...prev, [name]: value }));
  };

  const handleSubmit = async () => {
    setError(null);

    // Validate required fields
    for (const field of fields) {
      if (field.required !== false && !values[field.name]?.trim()) {
        setError(`"${field.label}" is required`);
        return;
      }
    }

    setSubmitting(true);
    try {
      await api.submitPhaseInput(taskId, phaseId, values);
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="flex flex-col flex-1 min-h-0 overflow-hidden">
      {/* Header */}
      <div className="px-6 py-3 border-b border-[var(--border-secondary)] flex items-center gap-2">
        <div className={`w-2 h-2 rounded-full ${isCompleted ? "bg-teal-500" : "bg-amber-500 animate-pulse"}`} />
        <span className="text-[13px] font-bold text-[var(--text-primary)]">{phaseName}</span>
        <span className="text-[12px] text-[var(--text-tertiary)]">{isCompleted ? "completed" : "awaiting input"}</span>
      </div>

      {/* Form */}
      <div className="flex-1 overflow-y-auto custom-scrollbar">
        <div className="max-w-lg mx-auto px-6 py-8">
          {isCompleted ? (
            <div className="py-6">
              <div className="text-center mb-6">
                <div className="w-12 h-12 bg-[#2EB57D] rounded-full flex items-center justify-center mx-auto mb-3">
                  <CheckCircle2 size={28} className="text-white" fill="white" stroke="#2EB57D" />
                </div>
                <p className="text-[14px] font-bold text-[var(--text-primary)] mb-1">Input submitted</p>
                <p className="text-[13px] text-[var(--text-secondary)]">Values from this phase are available to subsequent phases.</p>
              </div>

              {/* Show submitted values */}
              <div className="flex flex-col gap-2">
                {fields.map((field) => {
                  const val = submittedValues?.[field.name] ?? values[field.name] ?? "";
                  return (
                    <div key={field.name} className="flex items-center gap-2.5 rounded-lg px-3 py-2 bg-[var(--pinned-bg)] dark:bg-[var(--bg-secondary)] border border-[var(--border-secondary)]">
                      <div className="flex flex-col min-w-0">
                        <span className="text-[10px] font-bold text-[var(--text-tertiary)] uppercase tracking-wider">{field.label}</span>
                        <span className="text-[12px] text-[var(--text-primary)] truncate">{val || "—"}</span>
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          ) : (
            <div className="flex flex-col gap-5">
              {fields.map((field) => (
                <div key={field.name} className="flex flex-col gap-1.5">
                  <label
                    htmlFor={`input-${field.name}`}
                    className="text-[14px] font-bold text-[var(--text-primary)]"
                  >
                    {field.label}
                    {field.required !== false && (
                      <span className="text-red-500 ml-0.5">*</span>
                    )}
                  </label>
                  {field.description && (
                    <p className="text-[12px] text-[var(--text-secondary)] leading-relaxed -mt-0.5">
                      {field.description}
                    </p>
                  )}
                  <input
                    id={`input-${field.name}`}
                    type="text"
                    value={values[field.name] ?? ""}
                    onChange={(e) => handleChange(field.name, e.target.value)}
                    placeholder={field.placeholder ?? ""}
                    className="w-full px-3 py-2.5 text-[14px] rounded-lg border border-[var(--border-primary)] bg-[var(--bg-primary)] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--input-focus-border)] focus:shadow-sm transition-colors"
                  />
                </div>
              ))}

              {error && (
                <p className="text-[13px] text-red-500">{error}</p>
              )}

              <button
                onClick={handleSubmit}
                disabled={submitting || !hasAllRequired}
                className="self-end flex items-center gap-2 px-5 py-2.5 rounded-lg font-bold text-[14px] bg-[#007A5A] text-white hover:bg-[#00684c] transition-colors cursor-pointer disabled:opacity-50 mt-2"
              >
                {submitting ? (
                  <>
                    <Loader2 size={16} className="animate-spin" />
                    Submitting...
                  </>
                ) : (
                  <>
                    Next
                    <ArrowRight size={16} />
                  </>
                )}
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
