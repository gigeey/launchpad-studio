import { create } from "zustand";

export interface AssignmentEditorModalState {
  mode: "create" | "edit";
  /** Owning agent. Always present in "create" mode; also carried in "edit"
   *  mode so the modal doesn't need a second lookup just to know which
   *  agent's assignment list to refresh after a save. */
  agentId: string | null;
  /** Set only in "edit" mode — the assignment being edited. */
  assignmentId: string | null;
  /** Optional prefill for the Cron trigger's date/time, e.g. when the modal
   *  is opened from a calendar cell the user clicked on. */
  seedCronDate?: string;
}

interface AssignmentEditorModalStore {
  state: AssignmentEditorModalState | null;
  /** Bumped once after every successful create/edit save. The modal itself is
   *  mounted a single time at the app shell root (so any view can open it),
   *  which means a page showing an assignment list can't rely on a local
   *  `onSaved` closure — it instead depends on this counter to know when to
   *  refetch. */
  savedAt: number;
  openCreate: (agentId?: string, seedCronDate?: string) => void;
  openEdit: (agentId: string, assignmentId: string) => void;
  /** Fills in the owner once picked from the in-modal "no agent yet" picker
   *  (create mode, opened with no `agentId` — e.g. the sidebar's "New
   *  assignment" button). No-op outside that case. */
  selectAgent: (agentId: string) => void;
  close: () => void;
  markSaved: () => void;
}

/** Drives the create/edit assignment modal. Same single-nullable-state shape
 *  as competenciesModalStore: a store holds just enough
 *  to identify what's being edited, and a modal mounted once at the app shell
 *  root renders whenever the state is non-null. */
export const useAssignmentEditorModalStore = create<AssignmentEditorModalStore>((set) => ({
  state: null,
  savedAt: 0,
  openCreate: (agentId, seedCronDate) =>
    set({ state: { mode: "create", agentId: agentId ?? null, assignmentId: null, seedCronDate } }),
  openEdit: (agentId, assignmentId) =>
    set({ state: { mode: "edit", agentId, assignmentId } }),
  selectAgent: (agentId) =>
    set((s) => (s.state ? { state: { ...s.state, agentId } } : s)),
  close: () => set({ state: null }),
  markSaved: () => set((s) => ({ savedAt: s.savedAt + 1 })),
}));
