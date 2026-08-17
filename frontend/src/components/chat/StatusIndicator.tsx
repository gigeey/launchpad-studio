interface StatusIndicatorProps {
  status: "sending" | "sent" | "delivered" | "seen" | "error";
}

export function StatusIndicator({ status }: StatusIndicatorProps) {
  if (status === "error") {
    return (
      <span className="inline-flex items-center text-[12px] text-red-500">
        <svg width="12" height="12" viewBox="0 0 12 12" fill="none">
          <circle cx="6" cy="6" r="5" stroke="currentColor" strokeWidth="1.5" />
          <line x1="6" y1="3.5" x2="6" y2="6.5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          <circle cx="6" cy="8.5" r="0.75" fill="currentColor" />
        </svg>
      </span>
    );
  }

  if (status === "sending") {
    return (
      <span className="inline-flex items-center text-[12px] text-[var(--text-tertiary)]">
        <svg
          className="animate-spin"
          width="12"
          height="12"
          viewBox="0 0 12 12"
          fill="none"
        >
          <circle
            cx="6"
            cy="6"
            r="5"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeDasharray="20"
            strokeDashoffset="10"
          />
        </svg>
      </span>
    );
  }

  const isSeen = status === "seen";
  const isDelivered = status === "delivered";

  // Use grey for sent/delivered, blue for seen
  const color = isSeen ? "var(--accent)" : "var(--text-tertiary)";

  return (
    <svg
      width="16"
      height="11"
      viewBox="0 0 36 24"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className="inline-block align-middle"
    >
      {/* Left Tick */}
      <path
        d="M3 13 L9 19 L23 5"
        stroke={color}
        strokeWidth="3.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      {/* Right Tick (only for delivered and seen) */}
      {(isDelivered || isSeen) && (
        <path
          d="M14 14 L19 19 L33 5"
          stroke={color}
          strokeWidth="3.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      )}
    </svg>
  );
}
