import { motion } from "framer-motion";

export function TypingIndicator({ emoji }: { emoji: string }) {
  return (
    <div className="inline-flex items-center gap-[8px] px-[10px] py-[6px] rounded-full bg-[var(--bg-secondary)]">
      <span className="text-[20px] leading-none select-none">{emoji}</span>
      <div className="flex items-center gap-[4px]">
        <span className="text-[12px] text-[var(--text-secondary)]">is typing</span>
        <div className="flex items-center gap-[3px] pt-[2px]">
          {[0, 1, 2].map((i) => (
            <motion.span
              key={i}
              className="block w-[5px] h-[5px] rounded-full bg-[var(--text-secondary)]"
              animate={{ y: [0, -4, 0] }}
              transition={{
                duration: 0.6,
                repeat: Infinity,
                delay: i * 0.2,
                ease: "easeInOut",
              }}
            />
          ))}
        </div>
      </div>
    </div>
  );
}
