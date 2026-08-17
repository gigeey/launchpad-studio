import { useState, useRef, useEffect, useMemo, useCallback } from "react";
import { createPortal } from "react-dom";
import { motion, AnimatePresence } from "framer-motion";
import { ChevronLeft, ChevronRight, Calendar, X } from "lucide-react";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface DateTimePickerProps {
    value: string; // datetime-local format: "2026-04-13T10:30"
    onChange: (val: string) => void;
    placeholder?: string;
    className?: string;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const DAYS = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
const MONTHS = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

function daysInMonth(year: number, month: number): number {
    return new Date(year, month + 1, 0).getDate();
}

function firstDayOfMonth(year: number, month: number): number {
    return new Date(year, month, 1).getDay();
}

function pad(n: number): string {
    return n.toString().padStart(2, "0");
}

function formatDisplay(value: string): string {
    if (!value) return "";
    const d = new Date(value);
    if (isNaN(d.getTime())) return "";
    return d.toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        year: "numeric",
        hour: "numeric",
        minute: "2-digit",
    });
}

function parseValue(value: string): { year: number; month: number; day: number; hour: number; minute: number } | null {
    if (!value) return null;
    const d = new Date(value);
    if (isNaN(d.getTime())) return null;
    return {
        year: d.getFullYear(),
        month: d.getMonth(),
        day: d.getDate(),
        hour: d.getHours(),
        minute: d.getMinutes(),
    };
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
// Component
// ---------------------------------------------------------------------------

export function DateTimePicker({ value, onChange, placeholder = "Select date & time", className }: DateTimePickerProps) {
    const [open, setOpen] = useState(false);
    const wrapperRef = useRef<HTMLDivElement>(null);
    const triggerRef = useRef<HTMLButtonElement>(null);
    const dropdownRef = useRef<HTMLDivElement>(null);
    const [dropdownPos, setDropdownPos] = useState<{ top: number; left: number }>({ top: 0, left: 0 });
    const [openAbove, setOpenAbove] = useState(false);

    // Calendar navigation state
    const parsed = parseValue(value);
    const now = new Date();
    const [viewYear, setViewYear] = useState(parsed?.year ?? now.getFullYear());
    const [viewMonth, setViewMonth] = useState(parsed?.month ?? now.getMonth());

    // Time state (12-hour)
    const initial12 = parsed ? to12(parsed.hour) : { hour: 12, period: "PM" as const };
    const [hour12, setHour12] = useState(initial12.hour);
    const [minute, setMinute] = useState(parsed?.minute ?? 0);
    const [period, setPeriod] = useState<"AM" | "PM">(initial12.period);

    // Free-form text state for time inputs (validated on blur)
    const [hourText, setHourText] = useState(String(initial12.hour));
    const [minuteText, setMinuteText] = useState(pad(parsed?.minute ?? 0));
    const [hourError, setHourError] = useState(false);
    const [minuteError, setMinuteError] = useState(false);

    // Selected date
    const [selectedYear, setSelectedYear] = useState(parsed?.year ?? 0);
    const [selectedMonth, setSelectedMonth] = useState(parsed?.month ?? -1);
    const [selectedDay, setSelectedDay] = useState(parsed?.day ?? 0);

    // Sync when value changes externally
    useEffect(() => {
        const p = parseValue(value);
        if (p) {
            setViewYear(p.year);
            setViewMonth(p.month);
            setSelectedYear(p.year);
            setSelectedMonth(p.month);
            setSelectedDay(p.day);
            const t = to12(p.hour);
            setHour12(t.hour);
            setHourText(String(t.hour));
            setHourError(false);
            setMinute(p.minute);
            setMinuteText(pad(p.minute));
            setMinuteError(false);
            setPeriod(t.period);
        }
    }, [value]);

    // Build the value string and call onChange
    const emitChange = useCallback(
        (y: number, m: number, d: number, h12: number, min: number, p: "AM" | "PM") => {
            if (d === 0) return;
            const h24 = to24(h12, p);
            const val = `${y}-${pad(m + 1)}-${pad(d)}T${pad(h24)}:${pad(min)}`;
            onChange(val);
        },
        [onChange],
    );

    // Position dropdown relative to trigger, clamped to viewport
    useEffect(() => {
        if (!open || !triggerRef.current) return;
        const updatePos = () => {
            const rect = triggerRef.current!.getBoundingClientRect();
            const dropdownHeight = 420;
            const dropdownWidth = 300;
            const pad = 8;

            // Try below first, fall back to above, then clamp to viewport
            let top = rect.bottom + 6;
            if (top + dropdownHeight > window.innerHeight - pad) {
                top = rect.top - dropdownHeight - 6;
            }
            // Final clamp so it never goes off-screen
            top = Math.max(pad, Math.min(top, window.innerHeight - dropdownHeight - pad));

            let left = rect.left;
            left = Math.max(pad, Math.min(left, window.innerWidth - dropdownWidth - pad));

            setOpenAbove(top < rect.top);
            setDropdownPos({ top, left });
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

    // Calendar grid
    const calendarRows = useMemo(() => {
        const totalDays = daysInMonth(viewYear, viewMonth);
        const startDay = firstDayOfMonth(viewYear, viewMonth);
        const rows: (number | null)[][] = [];
        let row: (number | null)[] = [];

        for (let i = 0; i < startDay; i++) row.push(null);
        for (let d = 1; d <= totalDays; d++) {
            row.push(d);
            if (row.length === 7) {
                rows.push(row);
                row = [];
            }
        }
        if (row.length > 0) {
            while (row.length < 7) row.push(null);
            rows.push(row);
        }
        return rows;
    }, [viewYear, viewMonth]);

    const isToday = (day: number) =>
        viewYear === now.getFullYear() && viewMonth === now.getMonth() && day === now.getDate();

    const isSelected = (day: number) =>
        viewYear === selectedYear && viewMonth === selectedMonth && day === selectedDay;

    const prevMonth = () => {
        if (viewMonth === 0) { setViewMonth(11); setViewYear(viewYear - 1); }
        else setViewMonth(viewMonth - 1);
    };

    const nextMonth = () => {
        if (viewMonth === 11) { setViewMonth(0); setViewYear(viewYear + 1); }
        else setViewMonth(viewMonth + 1);
    };

    const selectDay = (day: number) => {
        setSelectedYear(viewYear);
        setSelectedMonth(viewMonth);
        setSelectedDay(day);
        emitChange(viewYear, viewMonth, day, hour12, minute, period);
    };

    const handleHourChange = (h: number) => {
        setHour12(h);
        emitChange(selectedYear, selectedMonth, selectedDay, h, minute, period);
    };

    const handleMinuteChange = (m: number) => {
        setMinute(m);
        emitChange(selectedYear, selectedMonth, selectedDay, hour12, m, period);
    };

    const handlePeriodChange = (p: "AM" | "PM") => {
        setPeriod(p);
        emitChange(selectedYear, selectedMonth, selectedDay, hour12, minute, p);
    };

    const handleClear = () => {
        onChange("");
        setSelectedDay(0);
        setSelectedMonth(-1);
        setSelectedYear(0);
        setOpen(false);
    };

    const timeInputClass =
        "w-[48px] h-[36px] px-2 text-center bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)]";

    return (
        <div ref={wrapperRef} className={`relative ${className ?? ""}`}>
            {/* Trigger button */}
            <button
                ref={triggerRef}
                type="button"
                onClick={() => setOpen(!open)}
                className="w-full h-[40px] px-3 flex items-center gap-2 bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] text-[15px] text-left focus:outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-colors cursor-pointer"
            >
                <Calendar size={16} className="text-[var(--modal-text-secondary)] flex-shrink-0" />
                <span className={value ? "text-[var(--modal-text-primary)]" : "text-[var(--modal-text-tertiary)]"}>
                    {value ? formatDisplay(value) : placeholder}
                </span>
            </button>

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
                            className="z-[9999] w-[300px] bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[10px] shadow-[0_8px_24px_rgba(0,0,0,0.15)] overflow-hidden"
                        >
                        {/* Month/Year nav */}
                        <div className="flex items-center justify-between px-4 pt-4 pb-2">
                            <button
                                type="button"
                                onClick={prevMonth}
                                className="p-1.5 rounded-[6px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                            >
                                <ChevronLeft size={16} />
                            </button>
                            <span className="text-[14px] font-bold text-[var(--modal-text-primary)]">
                                {MONTHS[viewMonth]} {viewYear}
                            </span>
                            <button
                                type="button"
                                onClick={nextMonth}
                                className="p-1.5 rounded-[6px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                            >
                                <ChevronRight size={16} />
                            </button>
                        </div>

                        {/* Day-of-week headers */}
                        <div className="grid grid-cols-7 px-3">
                            {DAYS.map((d) => (
                                <div key={d} className="h-[28px] flex items-center justify-center text-[11px] font-medium text-[var(--modal-text-secondary)]">
                                    {d}
                                </div>
                            ))}
                        </div>

                        {/* Calendar grid */}
                        <div className="px-3 pb-2">
                            {calendarRows.map((row, ri) => (
                                <div key={ri} className="grid grid-cols-7">
                                    {row.map((day, ci) => (
                                        <div key={ci} className="flex items-center justify-center h-[36px]">
                                            {day !== null && (
                                                <button
                                                    type="button"
                                                    onClick={() => selectDay(day)}
                                                    className={`w-[32px] h-[32px] rounded-full text-[13px] font-medium transition-colors cursor-pointer
                                                        ${isSelected(day)
                                                            ? "bg-[var(--success)] text-white"
                                                            : isToday(day)
                                                                ? "border border-[var(--success)] text-[var(--success)] hover:bg-[var(--success)]/10"
                                                                : "text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                                        }`}
                                                >
                                                    {day}
                                                </button>
                                            )}
                                        </div>
                                    ))}
                                </div>
                            ))}
                        </div>

                        {/* Divider */}
                        <div className="border-t border-[var(--modal-border-secondary)]" />

                        {/* Time inputs */}
                        <div className="flex items-center justify-center gap-2 px-4 py-3">
                            {/* Hour */}
                            <input
                                type="text"
                                inputMode="numeric"
                                value={hourText}
                                onChange={(e) => {
                                    const raw = e.target.value.replace(/[^0-9]/g, "").slice(0, 2);
                                    setHourText(raw);
                                    setHourError(false);
                                }}
                                onBlur={() => {
                                    const n = parseInt(hourText, 10);
                                    if (isNaN(n) || n < 1 || n > 12) {
                                        setHourError(true);
                                    } else {
                                        setHourError(false);
                                        setHourText(String(n));
                                        handleHourChange(n);
                                    }
                                }}
                                onFocus={(e) => { const el = e.target; requestAnimationFrame(() => el.select()); }}
                                onClick={(e) => { const el = e.target as HTMLInputElement; requestAnimationFrame(() => el.select()); }}
                                className={`${timeInputClass} ${hourError ? "!border-[var(--error)] !shadow-[0_0_0_1px_var(--error)]" : ""}`}
                                maxLength={2}
                            />

                            <span className="text-[16px] font-bold text-[var(--modal-text-secondary)]">:</span>

                            {/* Minute */}
                            <input
                                type="text"
                                inputMode="numeric"
                                value={minuteText}
                                onChange={(e) => {
                                    const raw = e.target.value.replace(/[^0-9]/g, "").slice(0, 2);
                                    setMinuteText(raw);
                                    setMinuteError(false);
                                }}
                                onBlur={() => {
                                    const n = parseInt(minuteText, 10);
                                    if (isNaN(n) || n < 0 || n > 59) {
                                        setMinuteError(true);
                                    } else {
                                        setMinuteError(false);
                                        setMinuteText(pad(n));
                                        handleMinuteChange(n);
                                    }
                                }}
                                onFocus={(e) => { const el = e.target; requestAnimationFrame(() => el.select()); }}
                                onClick={(e) => { const el = e.target as HTMLInputElement; requestAnimationFrame(() => el.select()); }}
                                className={`${timeInputClass} ${minuteError ? "!border-[var(--error)] !shadow-[0_0_0_1px_var(--error)]" : ""}`}
                                maxLength={2}
                            />

                            {/* AM/PM toggle */}
                            <div className="flex rounded-[8px] border border-[var(--modal-border-primary)] overflow-hidden">
                                {(["AM", "PM"] as const).map((p) => (
                                    <button
                                        key={p}
                                        type="button"
                                        onClick={() => handlePeriodChange(p)}
                                        className={`h-[36px] px-3 text-[13px] font-bold transition-colors cursor-pointer
                                            ${period === p
                                                ? "bg-[var(--success)] text-white"
                                                : "bg-[var(--modal-bg)] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                            }`}
                                    >
                                        {p}
                                    </button>
                                ))}
                            </div>
                        </div>

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
                                onClick={() => setOpen(false)}
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
