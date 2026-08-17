import { useState, useEffect, useRef } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Play, Pause } from "lucide-react";
import { agentAvatarColor } from "../../lib/agentColors";
import type { TaskStatus } from "../../types/workflow";

interface TaskProgressRingProps {
  completedPhases: number;
  totalPhases: number;
  workflowName: string;
  taskName: string;
  isDark?: boolean;
  /** ISO string or Date — when the task started. Falls back to component mount time. */
  startTime?: string | Date;
  /** ISO string or Date — when the task finished. If set, the timer freezes at this time. */
  endTime?: string | Date | null;
  /** Override the ring size in px. Defaults to 72. */
  ringSize?: number;
  /** Task status — controls hover behavior (pending shows play button). */
  status?: TaskStatus;
  /** Called when user clicks play on a pending task. */
  onStart?: () => void;
  /** Called when user clicks the task name label. */
  onClickName?: () => void;
  /** True when the task is running but current phase is paused (needs user attention). */
  isPaused?: boolean;
}

function formatElapsed(totalSeconds: number): string {
  const h = Math.floor(totalSeconds / 3600);
  const m = Math.floor((totalSeconds % 3600) / 60);
  const s = totalSeconds % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  if (h >= 1) {
    return `${h}Hr ${pad(m)}`;
  }
  return `${pad(m)}:${pad(s)}`;
}

/**
 * iMessage pinned chat view mixed with a segmented Activity Ring style.
 * On hover: ticks collapse inward, a solid circle fades in showing elapsed time,
 * and a second-ticker rotates on the outer ring.
 */
export function TaskProgressRing({
  completedPhases,
  totalPhases,
  workflowName,
  taskName,
  isDark,
  startTime,
  endTime,
  ringSize = 72,
  status,
  onStart,
  onClickName,
  isPaused,
}: TaskProgressRingProps) {
  const size = ringSize;
  // All geometry is in a fixed 72-unit coordinate space.
  // The browser scales the SVG cleanly via width/height vs viewBox.
  const BASE = 72;
  const center = BASE / 2;

  const numBars = 36;
  const outerRadius = 33.5;
  const innerRadius = 26;
  const barThickness = 3.5;
  const scale = size / BASE; // only used for font sizes, not SVG geometry

  // Hover state
  const [isHovered, setIsHovered] = useState(false);

  // Elapsed time
  const mountTime = useRef(Date.now());
  const [elapsedSeconds, setElapsedSeconds] = useState(0);

  const isFinished = status === "completed" || status === "failed" || status === "stopped";

  useEffect(() => {
    const origin = startTime ? new Date(startTime).getTime() : mountTime.current;
    // If task is finished, freeze the timer at the final duration
    if (endTime) {
      const end = new Date(endTime).getTime();
      setElapsedSeconds(Math.max(0, Math.floor((end - origin) / 1000)));
      return;
    }
    // Task is done but no completed_at — freeze at current elapsed (don't keep ticking)
    if (isFinished) {
      setElapsedSeconds(Math.floor((Date.now() - origin) / 1000));
      return;
    }
    const tick = () => setElapsedSeconds(Math.floor((Date.now() - origin) / 1000));
    tick();
    const id = setInterval(tick, 1000);
    return () => clearInterval(id);
  }, [startTime, endTime, isFinished]);

  // Second ticker rotation (6° per second, full rotation per minute)
  const [secondAngle, setSecondAngle] = useState(0);
  useEffect(() => {
    const tick = () => {
      const now = new Date();
      const seconds = now.getSeconds() + now.getMilliseconds() / 1000;
      setSecondAngle(seconds * 6); // 360° / 60s = 6°/s
    };
    tick();
    const id = setInterval(tick, 50); // smooth rotation
    return () => clearInterval(id);
  }, []);

  const tickLines = Array.from({ length: numBars }).map((_, i) => {
    const angleDeg = (i * 360) / numBars;
    const rad = ((angleDeg - 90) * Math.PI) / 180;
    return {
      x1: center + innerRadius * Math.cos(rad),
      y1: center + innerRadius * Math.sin(rad),
      x2: center + outerRadius * Math.cos(rad),
      y2: center + outerRadius * Math.sin(rad),
      key: i,
    };
  });

  // Progress calculations — using strokeDashoffset for smooth animation
  const progress = totalPhases > 0 ? Math.min(completedPhases / totalPhases, 1) : 0;

  const progressArcRadius = (innerRadius + outerRadius) / 2;
  const progressArcStrokeWidth = outerRadius - innerRadius + barThickness + 4;
  const circumference = 2 * Math.PI * progressArcRadius;

  const avatarBg = agentAvatarColor(taskName, isDark);

  const pausedColor = isDark ? "#fbbf24" : "#d97706"; // amber
  const colorIndex = (taskName || "").length % 6;
  const RING_COLORS = [
    isDark ? "#818cf8" : "#6366f1",
    isDark ? "#34d399" : "#10b981",
    isDark ? "#fb7185" : "#f43f5e",
    isDark ? "#38bdf8" : "#0ea5e9",
    isDark ? "#f87171" : "#ef4444",
    isDark ? "#fb923c" : "#f97316",
  ];
  const ringColor = isPaused ? pausedColor : RING_COLORS[colorIndex];

  const baseId = `ring-${taskName.replace(/[^a-zA-Z0-9]/g, "-")}`;
  const ticksMaskId = `${baseId}-ticks-mask`;
  const progressMaskId = `${baseId}-progress-mask`;

  const trackColor = isDark ? "rgba(255,255,255,0.06)" : "rgba(0,0,0,0.05)";
  const textColor = isDark ? "#ffffff" : "rgba(0,0,0,0.85)";
  const percentage = Math.round(progress * 100);

  // Second ticker geometry — a single line on the outer edge
  const tickerOuterR = outerRadius + 2;
  const tickerInnerR = outerRadius - 1;
  const tickerRad = ((secondAngle - 90) * Math.PI) / 180;
  const tickerX1 = center + tickerInnerR * Math.cos(tickerRad);
  const tickerY1 = center + tickerInnerR * Math.sin(tickerRad);
  const tickerX2 = center + tickerOuterR * Math.cos(tickerRad);
  const tickerY2 = center + tickerOuterR * Math.sin(tickerRad);

  const isPending = status === "pending";
  const isRunning = status === "running" && !isPaused;

  // Solid circle radius for hover state
  const solidCircleRadius = innerRadius - 2;

  return (
    <div className="inline-flex flex-col items-center gap-[4px] group cursor-default pt-[2px] min-w-0 w-full">
      {/* Workflow Name */}
      <span
        className="uppercase font-bold tracking-widest text-[var(--text-secondary)] opacity-80 px-1 truncate max-w-full"
        style={{ fontSize: Math.max(7, 9 * scale) }}
      >
        {workflowName}
      </span>

      {/* Avatar / Ring Container */}
      <div
        className="relative flex items-center justify-center rounded-full transition-transform duration-300 ease-out"
        style={{ width: size, height: size, background: avatarBg, overflow: "visible" }}
        onMouseEnter={() => setIsHovered(true)}
        onMouseLeave={() => setIsHovered(false)}
      >
        <svg
          width={size}
          height={size}
          viewBox={`0 0 ${BASE} ${BASE}`}
          className="absolute inset-0"
          style={{ overflow: "visible" }}
        >
          <defs>
            <mask id={ticksMaskId}>
              {tickLines.map((tick) => (
                <line
                  key={tick.key}
                  x1={tick.x1}
                  y1={tick.y1}
                  x2={tick.x2}
                  y2={tick.y2}
                  stroke="white"
                  strokeWidth={barThickness}
                  strokeLinecap="round"
                />
              ))}
            </mask>
            <mask id={progressMaskId}>
              <motion.circle
                cx={center}
                cy={center}
                r={progressArcRadius}
                fill="none"
                stroke="white"
                strokeWidth={progressArcStrokeWidth}
                strokeLinecap="butt"
                strokeDasharray={circumference}
                initial={{ strokeDashoffset: circumference }}
                animate={{ strokeDashoffset: circumference * (1 - progress) }}
                transition={{ duration: 0.8, ease: [0.4, 0, 0.2, 1] }}
                style={{ transform: `rotate(-90deg)`, transformOrigin: `${center}px ${center}px` }}
              />
            </mask>
          </defs>

          {/* Progress ticks — collapse inward on hover */}
          <motion.g
            mask={`url(#${ticksMaskId})`}
            animate={{
              scale: isHovered ? 0.55 : 1,
              opacity: isHovered ? 0 : 1,
            }}
            transition={{ duration: 0.35, ease: [0.4, 0, 0.2, 1] }}
            style={{ transformOrigin: `${center}px ${center}px` }}
          >
            <rect x="0" y="0" width={BASE} height={BASE} fill={trackColor} className="transition-colors duration-300" />
            {progress > 0 && (
              <rect x="0" y="0" width={BASE} height={BASE} fill={ringColor} mask={`url(#${progressMaskId})`} />
            )}
          </motion.g>

          {/* Solid circle — fades in on hover */}
          <motion.circle
            cx={center}
            cy={center}
            r={solidCircleRadius}
            fill={ringColor}
            initial={false}
            animate={{
              opacity: isHovered ? 0.9 : 0,
              scale: isHovered ? 1 : 0.7,
            }}
            transition={{ duration: 0.3, ease: [0.4, 0, 0.2, 1] }}
            style={{ transformOrigin: `${center}px ${center}px` }}
          />

          {/* Outer thin ring — visible on hover to frame the second ticker */}
          <motion.circle
            cx={center}
            cy={center}
            r={outerRadius + 0.5}
            fill="none"
            stroke={ringColor}
            strokeWidth={1}
            initial={false}
            animate={{ opacity: isHovered ? 0.4 : 0 }}
            transition={{ duration: 0.3 }}
          />

          {/* Pulsing attention ring when paused */}
          {isPaused && !isHovered && (
            <motion.circle
              cx={center}
              cy={center}
              r={outerRadius + 3}
              fill="none"
              stroke={pausedColor}
              strokeWidth={1.5}
              animate={{ opacity: [0.3, 0.8, 0.3], scale: [1, 1.04, 1] }}
              transition={{ duration: 2, repeat: Infinity, ease: "easeInOut" }}
              style={{ transformOrigin: `${center}px ${center}px` }}
            />
          )}

          {/* Pulsing ring when running (theme-colored) */}
          {isRunning && !isHovered && (
            <motion.circle
              cx={center}
              cy={center}
              r={outerRadius + 3}
              fill="none"
              stroke="var(--accent)"
              strokeWidth={1.5}
              animate={{ opacity: [0.3, 0.8, 0.3], scale: [1, 1.04, 1] }}
              transition={{ duration: 2, repeat: Infinity, ease: "easeInOut" }}
              style={{ transformOrigin: `${center}px ${center}px` }}
            />
          )}

          {/* Second ticker — only visible on hover */}
          <motion.line
            x1={tickerX1}
            y1={tickerY1}
            x2={tickerX2}
            y2={tickerY2}
            stroke={isDark ? "#fb923c" : "#f97316"}
            strokeWidth={2.5}
            strokeLinecap="round"
            initial={false}
            animate={{ opacity: isHovered ? 1 : 0 }}
            transition={{ duration: 0.2 }}
          />
        </svg>

        {/* Center content — switches between progress and timer/play */}
        <div className="absolute inset-0 flex flex-col items-center justify-center pt-[1px]">
          <AnimatePresence mode="wait">
            {isHovered ? (
              isPending ? (
                <motion.div
                  key="play"
                  className="flex items-center justify-center cursor-pointer"
                  initial={{ opacity: 0, scale: 0.85 }}
                  animate={{ opacity: 1, scale: 1 }}
                  exit={{ opacity: 0, scale: 0.85 }}
                  transition={{ duration: 0.2 }}
                  onClick={(e) => {
                    e.stopPropagation();
                    onStart?.();
                  }}
                >
                  <Play
                    className="text-white fill-white"
                    style={{ width: Math.max(14, 20 * scale), height: Math.max(14, 20 * scale) }}
                  />
                </motion.div>
              ) : (
                <motion.div
                  key="timer"
                  className="flex items-center justify-center"
                  initial={{ opacity: 0, scale: 0.85 }}
                  animate={{ opacity: 1, scale: 1 }}
                  exit={{ opacity: 0, scale: 0.85 }}
                  transition={{ duration: 0.2 }}
                >
                  <span
                    className="font-mono font-bold tracking-tight"
                    style={{ fontSize: Math.max(7, 10 * scale), color: "#ffffff", lineHeight: 1 }}
                  >
                    {formatElapsed(elapsedSeconds)}
                  </span>
                </motion.div>
              )
            ) : isPaused ? (
              <motion.div
                key="paused"
                className="flex flex-col items-center"
                initial={{ opacity: 0, scale: 0.85 }}
                animate={{ opacity: 1, scale: 1 }}
                exit={{ opacity: 0, scale: 0.85 }}
                transition={{ duration: 0.3 }}
              >
                <Pause
                  style={{ width: Math.max(12, 16 * scale), height: Math.max(12, 16 * scale), color: pausedColor }}
                  className="fill-current"
                />
                <motion.span
                  className="font-bold uppercase tracking-wide"
                  style={{ fontSize: Math.max(6, 8 * scale), color: pausedColor, lineHeight: 1, marginTop: 1.5 * scale }}
                  animate={{ opacity: [0.6, 1, 0.6] }}
                  transition={{ duration: 2, repeat: Infinity, ease: "easeInOut" }}
                >
                  Paused
                </motion.span>
              </motion.div>
            ) : (
              <motion.div
                key={`progress-${completedPhases}-${totalPhases}`}
                className="flex flex-col items-center"
                initial={{ opacity: 0, scale: 0.85 }}
                animate={{ opacity: 1, scale: 1 }}
                exit={{ opacity: 0, scale: 0.85 }}
                transition={{ duration: 0.3 }}
              >
                <div className="flex items-baseline gap-[1px] leading-none">
                  <span
                    className="font-bold tracking-tight"
                    style={{ fontSize: Math.max(12, 20 * scale), color: textColor, lineHeight: 1 }}
                  >
                    {completedPhases}
                  </span>
                  <span className="font-semibold opacity-70" style={{ fontSize: Math.max(8, 12 * scale), color: textColor, lineHeight: 1 }}>
                    /{totalPhases}
                  </span>
                </div>
                <span className="font-bold opacity-60" style={{ fontSize: Math.max(7, 10 * scale), color: textColor, lineHeight: 1, marginTop: 2.5 * scale }}>
                  {percentage}%
                </span>
              </motion.div>
            )}
          </AnimatePresence>
        </div>
      </div>

      {/* Task Name below — clickable */}
      <span
        className={`font-medium text-[var(--text-primary)] text-center leading-[1.2] line-clamp-2 px-1 max-w-full ${onClickName ? "cursor-pointer hover:underline" : ""}`}
        style={{ fontSize: Math.max(9, 12 * scale) }}
        onClick={onClickName}
      >
        {taskName}
      </span>
    </div>
  );
}
