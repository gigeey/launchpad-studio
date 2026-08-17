import type { AssignmentThreadPolicy } from "../../types/api";

/** Segmented-control options for `Assignment.thread_policy`, used by the
 *  trigger-aware AssignmentEditorModal. */
export const THREAD_POLICY_OPTIONS: { value: AssignmentThreadPolicy; label: string; caption: string }[] = [
  {
    value: "fresh",
    label: "New thread each run",
    caption: "A fresh, disposable thread every time it fires — never interrupts an active conversation.",
  },
  {
    value: "main",
    label: "Main thread",
    caption: "Posts into this agent's main conversation — good for a coach-style check-in.",
  },
  {
    value: "dedicated",
    label: "Dedicated thread",
    caption: "Reuses one ongoing thread across every run — good for a recurring brief that builds on itself.",
  },
];
