import { create } from "zustand";
import { persist } from "zustand/middleware";

// `focusPaths` maps a conversation key → the working dir (project/branch) that
// conversation's next message runs against. The key is NOT always a bare agent
// id: the Chat tab scopes it per thread (`agentId` for the default/main thread,
// `agentId:threadId` for the rest — see `threadDraftKey`), and Teams uses
// `team:{teamId}`. Keep the store key-agnostic; callers own the key scheme.
interface FocusPathState {
  focusPaths: Record<string, string>;
  setFocusPath: (key: string, path: string) => void;
  clearFocusPath: (key: string) => void;
}

export const useFocusPathStore = create<FocusPathState>()(
  persist(
    (set) => ({
      focusPaths: {},
      setFocusPath: (key, path) =>
        set((state) => ({
          focusPaths: { ...state.focusPaths, [key]: path },
        })),
      clearFocusPath: (key) =>
        set((state) => {
          const { [key]: _, ...rest } = state.focusPaths;
          return { focusPaths: rest };
        }),
    }),
    { name: "focus-paths" }
  )
);
