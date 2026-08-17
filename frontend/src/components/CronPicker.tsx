import { useState, useEffect, useCallback, useRef, useMemo } from "react";
import { createPortal } from "react-dom";
import { motion, AnimatePresence } from "framer-motion";
import { Repeat, X } from "lucide-react";
import cronstrue from "cronstrue";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface CronPickerProps {
    value: string;
    onChange: (cronExpr: string) => void;
    placeholder?: string;
}

type Frequency = "minute" | "hourly" | "daily" | "weekly" | "monthly";
type Mode = "visual" | "raw";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function pad(n: number): string {
    return n.toString().padStart(2, "0");
}

function cronDescription(expr: string): string | null {
    try {
        return cronstrue.toString(expr, { use24HourTimeFormat: false });
    } catch {
        return null;
    }
}

function to24(hour12: number, period: "AM" | "PM"): number {
    if (period === "AM") return hour12 === 12 ? 0 : hour12;
    return hour12 === 12 ? 12 : hour12 + 12;
}

function to12(hour24: number): { hour: number; period: "AM" | "PM" } {
    if (hour24 === 0) return { hour: 12, period: "AM" };
    if (hour24 < 12) return { hour: hour24, period: "AM" };
    if (hour24 === 12) return { hour: 12, period: "PM" };
    return { hour: hour24 - 12, period: "PM" };
}

// ---------------------------------------------------------------------------
// Cron builder
// ---------------------------------------------------------------------------

const DAY_LABELS = ["S", "M", "T", "W", "T", "F", "S"];
const DAY_NAMES = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

function buildCron(
    freq: Frequency,
    interval: number,
    days: boolean[],
    hour24: number,
    minute: number,
    dayOfMonth: number,
): string {
    switch (freq) {
        case "minute":
            return interval <= 1 ? "* * * * *" : `*/${interval} * * * *`;
        case "hourly":
            return interval <= 1
                ? `${minute} * * * *`
                : `${minute} */${interval} * * *`;
        case "daily":
            return interval <= 1
                ? `${minute} ${hour24} * * *`
                : `${minute} ${hour24} */${interval} * *`;
        case "weekly": {
            const selectedDays = days
                .map((sel, i) => (sel ? i : -1))
                .filter((i) => i >= 0);
            if (selectedDays.length === 0) return `${minute} ${hour24} * * *`;
            return `${minute} ${hour24} * * ${selectedDays.join(",")}`;
        }
        case "monthly":
            return interval <= 1
                ? `${minute} ${hour24} ${dayOfMonth} * *`
                : `${minute} ${hour24} ${dayOfMonth} */${interval} *`;
    }
}

// ---------------------------------------------------------------------------
// Cron parser (best-effort)
// ---------------------------------------------------------------------------

interface ParsedVisual {
    freq: Frequency;
    interval: number;
    days: boolean[];
    hour24: number;
    minute: number;
    dayOfMonth: number;
}

function parseCronToVisual(expr: string): ParsedVisual | null {
    const parts = expr.trim().split(/\s+/);
    if (parts.length !== 5) return null;

    const [minField, hrField, domField, monField, dowField] = parts;

    // Every N minutes: */N * * * *
    if (hrField === "*" && domField === "*" && monField === "*" && dowField === "*") {
        if (minField === "*") return { freq: "minute", interval: 1, days: Array(7).fill(false), hour24: 0, minute: 0, dayOfMonth: 1 };
        if (minField.startsWith("*/")) {
            const n = parseInt(minField.slice(2));
            if (!isNaN(n)) return { freq: "minute", interval: n, days: Array(7).fill(false), hour24: 0, minute: 0, dayOfMonth: 1 };
        }
    }

    // Hourly: M */N * * * or M * * * *
    if (domField === "*" && monField === "*" && dowField === "*" && !isNaN(parseInt(minField))) {
        const min = parseInt(minField);
        if (hrField === "*") return { freq: "hourly", interval: 1, days: Array(7).fill(false), hour24: 0, minute: min, dayOfMonth: 1 };
        if (hrField.startsWith("*/")) {
            const n = parseInt(hrField.slice(2));
            if (!isNaN(n)) return { freq: "hourly", interval: n, days: Array(7).fill(false), hour24: 0, minute: min, dayOfMonth: 1 };
        }
    }

    // Weekly: M H * * D,D,D
    if (domField === "*" && monField === "*" && dowField !== "*" && !isNaN(parseInt(minField)) && !isNaN(parseInt(hrField))) {
        const min = parseInt(minField);
        const hr = parseInt(hrField);
        const days = Array(7).fill(false) as boolean[];
        const dayParts = dowField.split(",");
        for (const dp of dayParts) {
            const d = parseInt(dp);
            if (!isNaN(d) && d >= 0 && d <= 6) days[d] = true;
        }
        return { freq: "weekly", interval: 1, days, hour24: hr, minute: min, dayOfMonth: 1 };
    }

    // Daily: M H */N * * or M H * * *  (with specific hour, no DOW)
    if (monField === "*" && dowField === "*" && !isNaN(parseInt(minField)) && !isNaN(parseInt(hrField))) {
        const min = parseInt(minField);
        const hr = parseInt(hrField);
        if (domField === "*") return { freq: "daily", interval: 1, days: Array(7).fill(false), hour24: hr, minute: min, dayOfMonth: 1 };
        if (domField.startsWith("*/")) {
            const n = parseInt(domField.slice(2));
            if (!isNaN(n)) return { freq: "daily", interval: n, days: Array(7).fill(false), hour24: hr, minute: min, dayOfMonth: 1 };
        }
        // Monthly: M H DOM * * or M H DOM */N *
        if (!isNaN(parseInt(domField))) {
            const dom = parseInt(domField);
            return { freq: "monthly", interval: 1, days: Array(7).fill(false), hour24: hr, minute: min, dayOfMonth: dom };
        }
    }

    // Monthly with interval: M H DOM */N *
    if (dowField === "*" && !isNaN(parseInt(minField)) && !isNaN(parseInt(hrField)) && !isNaN(parseInt(domField))) {
        const min = parseInt(minField);
        const hr = parseInt(hrField);
        const dom = parseInt(domField);
        if (monField.startsWith("*/")) {
            const n = parseInt(monField.slice(2));
            if (!isNaN(n)) return { freq: "monthly", interval: n, days: Array(7).fill(false), hour24: hr, minute: min, dayOfMonth: dom };
        }
    }

    return null;
}

// ---------------------------------------------------------------------------
// Shared styles
// ---------------------------------------------------------------------------

const selectClass =
    "h-[36px] px-2 bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] appearance-none cursor-pointer";

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function CronPicker({ value, onChange, placeholder = "Set schedule" }: CronPickerProps) {
    const [open, setOpen] = useState(false);
    const triggerRef = useRef<HTMLButtonElement>(null);
    const dropdownRef = useRef<HTMLDivElement>(null);
    const wrapperRef = useRef<HTMLDivElement>(null);
    const [dropdownPos, setDropdownPos] = useState<{ top: number; left: number }>({ top: 0, left: 0 });
    const [openAbove, setOpenAbove] = useState(false);

    const [mode, setMode] = useState<Mode>("visual");

    // Visual state
    const [freq, setFreq] = useState<Frequency>("daily");
    const [interval, setInterval] = useState(1);
    const [days, setDays] = useState<boolean[]>(Array(7).fill(false));
    const [hour12, setHour12] = useState(9);
    const [minute, setMinute] = useState(0);
    const [period, setPeriod] = useState<"AM" | "PM">("AM");
    const [dayOfMonth, setDayOfMonth] = useState(1);

    // Raw input (only used in raw mode)
    const [rawValue, setRawValue] = useState(value);

    // Track whether we're updating internally to avoid loops
    const [internalUpdate, setInternalUpdate] = useState(false);

    // Parse incoming value into visual state
    useEffect(() => {
        if (internalUpdate) {
            setInternalUpdate(false);
            return;
        }
        if (!value) return;
        setRawValue(value);
        const parsed = parseCronToVisual(value);
        if (parsed) {
            setFreq(parsed.freq);
            setInterval(parsed.interval);
            setDays(parsed.days);
            const t = to12(parsed.hour24);
            setHour12(t.hour);
            setPeriod(t.period);
            setMinute(parsed.minute);
            setDayOfMonth(parsed.dayOfMonth);
        } else if (mode === "visual") {
            setMode("raw");
        }
    }, [value]); // eslint-disable-line react-hooks/exhaustive-deps

    // Emit changes from visual controls
    const emitVisual = useCallback(
        (f: Frequency, iv: number, d: boolean[], h12: number, min: number, p: "AM" | "PM", dom: number) => {
            const h24 = to24(h12, p);
            const cron = buildCron(f, iv, d, h24, min, dom);
            setInternalUpdate(true);
            setRawValue(cron);
            onChange(cron);
        },
        [onChange],
    );

    // Frequency change
    const handleFreqChange = (f: Frequency) => {
        setFreq(f);
        emitVisual(f, interval, days, hour12, minute, period, dayOfMonth);
    };

    // Interval change
    const handleIntervalChange = (n: number) => {
        const v = Math.max(1, n);
        setInterval(v);
        emitVisual(freq, v, days, hour12, minute, period, dayOfMonth);
    };

    // Day toggle
    const handleDayToggle = (i: number) => {
        const nd = [...days];
        nd[i] = !nd[i];
        setDays(nd);
        emitVisual(freq, interval, nd, hour12, minute, period, dayOfMonth);
    };

    // Time changes
    const handleHourChange = (h: number) => {
        setHour12(h);
        emitVisual(freq, interval, days, h, minute, period, dayOfMonth);
    };
    const handleMinuteChange = (m: number) => {
        setMinute(m);
        emitVisual(freq, interval, days, hour12, m, period, dayOfMonth);
    };
    const handlePeriodChange = (p: "AM" | "PM") => {
        setPeriod(p);
        emitVisual(freq, interval, days, hour12, minute, p, dayOfMonth);
    };

    // Day of month
    const handleDomChange = (d: number) => {
        setDayOfMonth(d);
        emitVisual(freq, interval, days, hour12, minute, period, d);
    };

    // Raw mode change
    const handleRawChange = (v: string) => {
        setRawValue(v);
        onChange(v);
    };

    // Switch to visual — try to parse current raw value
    const switchToVisual = () => {
        const parsed = parseCronToVisual(rawValue);
        if (parsed) {
            setFreq(parsed.freq);
            setInterval(parsed.interval);
            setDays(parsed.days);
            const t = to12(parsed.hour24);
            setHour12(t.hour);
            setPeriod(t.period);
            setMinute(parsed.minute);
            setDayOfMonth(parsed.dayOfMonth);
        }
        setMode("visual");
    };

    const handleClear = () => {
        onChange("");
        setRawValue("");
        setOpen(false);
    };

    const desc = useMemo(() => (value ? cronDescription(value) : null), [value]);

    // Position dropdown relative to trigger
    useEffect(() => {
        if (!open || !triggerRef.current) return;
        const updatePos = () => {
            const rect = triggerRef.current!.getBoundingClientRect();
            const dropdownHeight = 340;
            const spaceBelow = window.innerHeight - rect.bottom;
            const spaceAbove = rect.top;
            const shouldOpenAbove = spaceBelow < dropdownHeight && spaceAbove > spaceBelow;
            setOpenAbove(shouldOpenAbove);
            setDropdownPos({
                top: shouldOpenAbove ? rect.top - dropdownHeight - 6 : rect.bottom + 6,
                left: rect.left,
            });
        };
        updatePos();
        window.addEventListener("scroll", updatePos, true);
        window.addEventListener("resize", updatePos);
        return () => {
            window.removeEventListener("scroll", updatePos, true);
            window.removeEventListener("resize", updatePos);
        };
    }, [open]);

    // Click outside to close
    useEffect(() => {
        if (!open) return;
        const handler = (e: MouseEvent) => {
            const target = e.target as Node;
            if (
                wrapperRef.current && !wrapperRef.current.contains(target) &&
                dropdownRef.current && !dropdownRef.current.contains(target)
            ) {
                setOpen(false);
            }
        };
        document.addEventListener("mousedown", handler);
        return () => document.removeEventListener("mousedown", handler);
    }, [open]);

    // Escape to close
    useEffect(() => {
        if (!open) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === "Escape") setOpen(false);
        };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [open]);

    const frequencyTabs: { key: Frequency; label: string }[] = [
        { key: "minute", label: "Minute" },
        { key: "hourly", label: "Hourly" },
        { key: "daily", label: "Daily" },
        { key: "weekly", label: "Weekly" },
        { key: "monthly", label: "Monthly" },
    ];

    const showInterval = freq === "minute" || freq === "hourly" || freq === "daily" || freq === "monthly";
    const showDays = freq === "weekly";
    const showTime = freq === "daily" || freq === "weekly" || freq === "monthly";
    const showMinuteOnly = freq === "hourly";
    const showDom = freq === "monthly";

    const intervalLabel = freq === "minute" ? "minute(s)" : freq === "hourly" ? "hour(s)" : freq === "daily" ? "day(s)" : "month(s)";

    return (
        <div ref={wrapperRef} className="relative">
            {/* Trigger button */}
            <button
                ref={triggerRef}
                type="button"
                onClick={() => setOpen(!open)}
                className="w-full h-[40px] px-3 flex items-center gap-2 bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] text-[15px] text-left focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors cursor-pointer"
            >
                <Repeat size={16} className="text-[var(--modal-text-secondary)] flex-shrink-0" />
                <span className={value && desc ? "text-[var(--modal-text-primary)]" : "text-[var(--modal-text-secondary)]"}>
                    {value && desc ? desc : placeholder}
                </span>
            </button>

            {/* Human-readable description below trigger */}
            {value && (
                <p className={`text-[13px] mt-1.5 ${desc ? "text-[var(--success)]" : "text-[var(--error)]"}`}>
                    {desc ?? "Invalid cron expression"}
                    {desc && <span className="text-[var(--modal-text-secondary)] ml-2 font-mono text-[11px]">({value})</span>}
                </p>
            )}

            {/* Dropdown (portal) */}
            {createPortal(
                <AnimatePresence>
                    {open && (
                        <motion.div
                            ref={dropdownRef}
                            initial={{ opacity: 0, y: openAbove ? 4 : -4 }}
                            animate={{ opacity: 1, y: 0 }}
                            exit={{ opacity: 0, y: openAbove ? 4 : -4 }}
                            transition={{ duration: 0.15 }}
                            style={{ position: "fixed", top: dropdownPos.top, left: dropdownPos.left }}
                            className="z-[9999] w-[360px] bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] shadow-[0_8px_24px_rgba(0,0,0,0.15)] overflow-hidden"
                        >
                            {/* Mode toggle */}
                            <div className="flex items-center justify-between px-4 pt-4 pb-3">
                                <span className="text-[14px] font-bold text-[var(--modal-text-primary)]">Schedule</span>
                                <div className="flex rounded-[8px] border border-[var(--modal-border-primary)] overflow-hidden">
                                    <button
                                        type="button"
                                        onClick={switchToVisual}
                                        className={`h-[26px] px-2.5 text-[11px] font-bold transition-colors cursor-pointer ${
                                            mode === "visual"
                                                ? "bg-[var(--success)] text-white"
                                                : "bg-[var(--modal-bg)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                        }`}
                                    >
                                        Visual
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => setMode("raw")}
                                        className={`h-[26px] px-2.5 text-[11px] font-bold transition-colors cursor-pointer ${
                                            mode === "raw"
                                                ? "bg-[var(--success)] text-white"
                                                : "bg-[var(--modal-bg)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                        }`}
                                    >
                                        Raw
                                    </button>
                                </div>
                            </div>

                            {/* Divider */}
                            <div className="border-t border-[var(--modal-border-secondary)]" />

                            <AnimatePresence mode="wait">
                                {mode === "raw" ? (
                                    /* Raw mode */
                                    <motion.div
                                        key="raw"
                                        initial={{ opacity: 0 }}
                                        animate={{ opacity: 1 }}
                                        exit={{ opacity: 0 }}
                                        transition={{ duration: 0.12 }}
                                        className="px-4 py-4"
                                    >
                                        <input
                                            type="text"
                                            value={rawValue}
                                            onChange={(e) => handleRawChange(e.target.value)}
                                            placeholder="e.g. 0 9 * * *"
                                            className="w-full h-[40px] px-3 bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] text-[14px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-secondary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors"
                                        />
                                    </motion.div>
                                ) : (
                                    /* Visual mode */
                                    <motion.div
                                        key="visual"
                                        initial={{ opacity: 0 }}
                                        animate={{ opacity: 1 }}
                                        exit={{ opacity: 0 }}
                                        transition={{ duration: 0.12 }}
                                        className="px-4 py-4 flex flex-col gap-4"
                                    >
                                        {/* Frequency tabs */}
                                        <div className="flex rounded-[8px] border border-[var(--modal-border-primary)] overflow-hidden">
                                            {frequencyTabs.map((tab) => (
                                                <button
                                                    key={tab.key}
                                                    type="button"
                                                    onClick={() => handleFreqChange(tab.key)}
                                                    className={`flex-1 h-[30px] text-[12px] font-medium transition-colors cursor-pointer ${
                                                        freq === tab.key
                                                            ? "bg-[var(--success)] text-white"
                                                            : "bg-[var(--modal-bg)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                                    }`}
                                                >
                                                    {tab.label}
                                                </button>
                                            ))}
                                        </div>

                                        {/* Interval */}
                                        {showInterval && (
                                            <div className="flex items-center gap-2">
                                                <span className="text-[13px] text-[var(--modal-text-primary)]">Every</span>
                                                <input
                                                    type="number"
                                                    min={1}
                                                    max={freq === "minute" ? 59 : freq === "hourly" ? 23 : freq === "daily" ? 31 : 12}
                                                    value={interval}
                                                    onChange={(e) => handleIntervalChange(parseInt(e.target.value) || 1)}
                                                    className="w-[54px] h-[32px] px-2 text-center bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[8px] text-[13px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors"
                                                />
                                                <span className="text-[13px] text-[var(--modal-text-primary)]">{intervalLabel}</span>
                                            </div>
                                        )}

                                        {/* Day circles (weekly) */}
                                        {showDays && (
                                            <div className="flex items-center gap-1.5">
                                                {DAY_LABELS.map((label, i) => (
                                                    <button
                                                        key={i}
                                                        type="button"
                                                        title={DAY_NAMES[i]}
                                                        onClick={() => handleDayToggle(i)}
                                                        className={`w-[34px] h-[34px] rounded-full text-[12px] font-bold transition-colors cursor-pointer ${
                                                            days[i]
                                                                ? "bg-[var(--success)] text-white"
                                                                : "bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                                        }`}
                                                    >
                                                        {label}
                                                    </button>
                                                ))}
                                            </div>
                                        )}

                                        {/* Day of month (monthly) */}
                                        {showDom && (
                                            <div className="flex items-center gap-2">
                                                <span className="text-[13px] text-[var(--modal-text-primary)]">on day</span>
                                                <select
                                                    value={dayOfMonth}
                                                    onChange={(e) => handleDomChange(parseInt(e.target.value))}
                                                    className={selectClass}
                                                >
                                                    {Array.from({ length: 31 }, (_, i) => i + 1).map((d) => (
                                                        <option key={d} value={d}>{d}</option>
                                                    ))}
                                                </select>
                                            </div>
                                        )}

                                        {/* Time picker */}
                                        {(showTime || showMinuteOnly) && (
                                            <div className="flex items-center gap-2">
                                                <span className="text-[13px] text-[var(--modal-text-primary)]">
                                                    {showMinuteOnly ? "at minute" : "at"}
                                                </span>

                                                {showTime && (
                                                    <>
                                                        <select
                                                            value={hour12}
                                                            onChange={(e) => handleHourChange(parseInt(e.target.value))}
                                                            className={selectClass}
                                                        >
                                                            {Array.from({ length: 12 }, (_, i) => i + 1).map((h) => (
                                                                <option key={h} value={h}>{h}</option>
                                                            ))}
                                                        </select>
                                                        <span className="text-[14px] font-bold text-[var(--modal-text-secondary)]">:</span>
                                                    </>
                                                )}

                                                <select
                                                    value={minute}
                                                    onChange={(e) => handleMinuteChange(parseInt(e.target.value))}
                                                    className={selectClass}
                                                >
                                                    {Array.from({ length: 60 }, (_, i) => i).map((m) => (
                                                        <option key={m} value={m}>{pad(m)}</option>
                                                    ))}
                                                </select>

                                                {showTime && (
                                                    <div className="flex rounded-[8px] border border-[var(--modal-border-primary)] overflow-hidden">
                                                        {(["AM", "PM"] as const).map((p) => (
                                                            <button
                                                                key={p}
                                                                type="button"
                                                                onClick={() => handlePeriodChange(p)}
                                                                className={`h-[36px] px-2.5 text-[12px] font-bold transition-colors cursor-pointer ${
                                                                    period === p
                                                                        ? "bg-[var(--success)] text-white"
                                                                        : "bg-[var(--modal-bg)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                                                }`}
                                                            >
                                                                {p}
                                                            </button>
                                                        ))}
                                                    </div>
                                                )}
                                            </div>
                                        )}
                                    </motion.div>
                                )}
                            </AnimatePresence>

                            {/* Divider */}
                            <div className="border-t border-[var(--modal-border-secondary)]" />

                            {/* Actions */}
                            <div className="flex items-center justify-between px-4 py-3">
                                <button
                                    type="button"
                                    onClick={handleClear}
                                    className="flex items-center gap-1 text-[13px] font-medium text-[var(--modal-text-secondary)] hover:text-[var(--error)] transition-colors cursor-pointer"
                                >
                                    <X size={14} />
                                    Clear
                                </button>
                                <button
                                    type="button"
                                    onClick={() => {
                                        if (mode === "visual") {
                                            emitVisual(freq, interval, days, hour12, minute, period, dayOfMonth);
                                        } else if (rawValue) {
                                            onChange(rawValue);
                                        }
                                        setOpen(false);
                                    }}
                                    className="px-4 py-1.5 rounded-[8px] bg-[var(--success)] text-white text-[13px] font-bold hover:brightness-110 transition-colors cursor-pointer"
                                >
                                    Done
                                </button>
                            </div>
                        </motion.div>
                    )}
                </AnimatePresence>,
                document.body,
            )}
        </div>
    );
}
