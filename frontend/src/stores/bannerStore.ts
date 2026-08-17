import { create } from "zustand";
import type { ReactNode } from "react";

export interface Banner {
  id: string;
  /** Higher priority shows first */
  priority: number;
  message: string | ReactNode;
  /** "warning" = yellow, "error" = red, "info" = blue */
  variant: "warning" | "error" | "info";
  dismissible: boolean;
  /** Optional action button */
  action?: { label: string; onClick: () => void };
}

interface BannerState {
  banners: Banner[];
  /** Set of banner IDs dismissed this session */
  dismissed: Set<string>;
  addBanner: (banner: Banner) => void;
  removeBanner: (id: string) => void;
  dismissBanner: (id: string) => void;
}

export const useBannerStore = create<BannerState>((set) => ({
  banners: [],
  dismissed: new Set(),

  addBanner: (banner) =>
    set((state) => {
      // Replace if same id exists
      const filtered = state.banners.filter((b) => b.id !== banner.id);
      return { banners: [...filtered, banner] };
    }),

  removeBanner: (id) =>
    set((state) => ({
      banners: state.banners.filter((b) => b.id !== id),
      dismissed: (() => {
        const next = new Set(state.dismissed);
        next.delete(id);
        return next;
      })(),
    })),

  dismissBanner: (id) =>
    set((state) => {
      const next = new Set(state.dismissed);
      next.add(id);
      return { dismissed: next };
    }),
}));
