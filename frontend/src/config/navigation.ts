import type { ComponentType } from "react";
import {
  SquareCheckBig,
  MessageSquare,
  Settings,
  Settings as SettingsIcon,
  FileText,
  KanbanSquare,
  Archive,
  FolderKanban,
  House,
  CalendarClock,
  FolderOpen,
  LayoutGrid,
} from "lucide-react";
import type { LucideProps } from "lucide-react";

// --- Types ---

export type ViewId = "home" | "tasks" | "projects" | "scheduled" | "chat" | "settings" | "assets";

export interface SubMenuItem {
  id: string;
  label: string;
  emoji?: string;
  icon?: ComponentType<LucideProps>;
  description?: string;
  isSectionHeader?: boolean;
}

export interface ViewConfig {
  id: ViewId;
  label: string;
  icon: ComponentType<LucideProps>;
  path: string;
  subMenuItems: SubMenuItem[];
}

// --- Configuration ---

export const viewConfigs: ViewConfig[] = [
  // {
  //   id: "home",
  //   label: "Home",
  //   icon: LayoutGrid,
  //   path: "/home",
  //   subMenuItems: [
  //     { id: "dashboard", label: "Dashboard", emoji: "🏠" },
  //     { id: "activity", label: "Activity", emoji: "📈" },
  //     { id: "favorites", label: "Favorites", emoji: "⭐" },
  //     { id: "getting-started", label: "Getting Started", emoji: "🚀" },
  //   ],
  // },
  {
    id: "home",
    label: "Home",
    icon: House,
    path: "/home",
    subMenuItems: [],
  },
  {
    id: "chat",
    label: "Chat",
    icon: MessageSquare,
    path: "/chat",
    subMenuItems: [],
  },
  {
    // NOTE: `id`/`path` stay "scheduled" during the convergence to avoid
    // churning the many `ViewId === "scheduled"` call sites (routing, sidebar
    // switch, sidebar-width keys); only the user-facing label becomes
    // "Assignments". A later slice may rename the route to /assignments.
    id: "scheduled",
    label: "Assignments",
    icon: CalendarClock,
    path: "/scheduled",
    subMenuItems: [],
  },
  // files
  {
    id: "assets",
    label: "Assets",
    icon: FolderOpen,
    path: "/assets",
    subMenuItems: [],
  },
  {
    // NOTE: `id`/`path` stay "tasks" during the convergence to avoid churning
    // the many `ViewId === "tasks"` call sites (routing, sidebar switch,
    // sidebar-width keys); only the user-facing label becomes "Workflows".
    id: "tasks",
    label: "Workflows",
    icon: SquareCheckBig,
    path: "/tasks",
    subMenuItems: [
      { id: "workflows", label: "Workflows", icon: LayoutGrid },
      { id: "board", label: "Board", icon: KanbanSquare },
      { id: "archived", label: "Archived", icon: Archive },
    ],
  },
  {
    id: "projects",
    label: "Projects",
    icon: FolderKanban,
    path: "/projects",
    subMenuItems: [],
  },
  {
    id: "settings",
    label: "Settings",
    icon: Settings,
    path: "/settings",
    subMenuItems: [
      { id: "general", label: "General", icon: SettingsIcon },
      { id: "docs", label: "Docs", icon: FileText },
    ],
  },
];

// --- Helpers ---

export function getViewConfig(viewId: ViewId): ViewConfig | undefined {
  return viewConfigs.find((config) => config.id === viewId);
}
