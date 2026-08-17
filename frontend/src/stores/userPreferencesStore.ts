import { create } from "zustand";
import { persist } from "zustand/middleware";
import { useSyncExternalStore } from "react";
import type { ViewId } from "../config/navigation";
import type { HomeChannelsGroupBy } from "../lib/homeChannelGrouping";
import type { HomeAssignmentsGroupBy } from "../lib/homeAssignmentGrouping";

export type ThemePreference = "light" | "dark" | "system";

// Each nav rail view remembers its own sidebar width. Chat defaults wide
// since it's the primary surface; Home is trimmed slightly so it doesn't
// compete visually. Everything else keeps the old shared default (320) so
// switching to per-view widths doesn't reflow views nobody has resized yet.
// Once a user drags a divider, that view's width lands in `sidebarWidths`
// and these defaults are only used until then.
export const DEFAULT_SIDEBAR_WIDTHS: Record<ViewId, number> = {
    home: 280,
    chat: 320,
    tasks: 180,
    projects: 320,
    scheduled: 320,
    assets: 320,
    settings: 320,
};

/** Resolves a view's sidebar width: user override if set, else its default. */
export function resolveSidebarWidth(
    widths: Partial<Record<ViewId, number>>,
    viewId: ViewId
): number {
    return widths[viewId] ?? DEFAULT_SIDEBAR_WIDTHS[viewId];
}

interface UserPreferencesState {
    sidebarWidths: Partial<Record<ViewId, number>>;
    setSidebarWidthForView: (viewId: ViewId, width: number) => void;
    memoryPanelWidth: number;
    setMemoryPanelWidth: (width: number) => void;
    // Width of the pinned Channels column (`ChannelsColumn`, left of the chat
    // area). Separate from `memoryPanelWidth` above — that's the unrelated
    // right-side chat drawer.
    channelsColumnWidth: number;
    setChannelsColumnWidth: (width: number) => void;
    // Width of the pinned Assignments column (Chat pill), same convention as
    // `channelsColumnWidth` above.
    assignmentsColumnWidth: number;
    setAssignmentsColumnWidth: (width: number) => void;
    // Width of the artifact "Adjust with chat" mini-thread panel
    // (`ArtifactChatPanel`). Separate from `memoryPanelWidth` above — that's
    // the unrelated chat-side memory drawer in `ChatView`.
    artifactChatPanelWidth: number;
    setArtifactChatPanelWidth: (width: number) => void;
    // Width of the agent-list column in the Settings → Memories panel (and
    // its popped-out window). Separate from `memoryPanelWidth` above, which
    // is the unrelated chat-side memory drawer.
    memoriesAgentListWidth: number;
    setMemoriesAgentListWidth: (width: number) => void;
    theme: ThemePreference;
    setTheme: (theme: ThemePreference) => void;
    // Default view for the aggregate Scheduled page. Calendar is the product
    // default (users think about scheduled work spatially by date first); the
    // flat list is the alternate. Persisted so the choice survives reloads.
    scheduledView: "calendar" | "list";
    setScheduledView: (view: "calendar" | "list") => void;
    // "Group by agent" switch on the Scheduled list view. Persisted so the
    // choice survives leaving/reentering the list (or a reload) instead of
    // silently resetting to the default every time.
    scheduledListGroupByAgent: boolean;
    setScheduledListGroupByAgent: (grouped: boolean) => void;
    font: string;
    setFont: (font: string) => void;
    bubbleColor: string;
    setBubbleColor: (color: string) => void;
    timezoneAuto: boolean;
    setTimezoneAuto: (auto: boolean) => void;
    timezone: string;
    setTimezone: (tz: string) => void;
    kanbanStatusFilters: string[];
    setKanbanStatusFilters: (filters: string[]) => void;
    kanbanWorkflowFilters: string[];
    setKanbanWorkflowFilters: (filters: string[]) => void;
    appTheme: string;
    setAppTheme: (appTheme: string) => void;
    // Raw 10-hex palette behind the "custom" appTheme, in role order (see
    // src/lib/customTheme.ts). Only one custom theme exists at a time —
    // saving a new palette overwrites this rather than appending to a list.
    // Null until the user has pasted a palette at least once.
    customThemeColors: string[] | null;
    setCustomThemeColors: (colors: string[] | null) => void;
    circularAvatars: boolean;
    setCircularAvatars: (circular: boolean) => void;
    showRecentTasks: boolean;
    setShowRecentTasks: (show: boolean) => void;
    homeJobsCollapsed: boolean;
    setHomeJobsCollapsed: (collapsed: boolean) => void;
    homeAgentsCollapsed: boolean;
    setHomeAgentsCollapsed: (collapsed: boolean) => void;
    // Which agents' nested-thread lists are expanded on the Home sidebar.
    // Stored as an array (Sets aren't JSON-serializable) — HomeSidebar reads
    // it into a Set for O(1) lookups and writes back through
    // `setHomeExpandedAgentIds`. Same persisted `user-preferences` store as
    // the two collapse flags above, so per-agent expand/collapse survives
    // navigating away from Home and back too.
    homeExpandedAgentIds: string[];
    setHomeExpandedAgentIds: (ids: string[]) => void;
    // Home "Channels" section — collapse state for the section header itself,
    // same convention as `homeAgentsCollapsed`/`homeJobsCollapsed` above.
    homeChannelsCollapsed: boolean;
    setHomeChannelsCollapsed: (collapsed: boolean) => void;
    // "By channel" (the default) vs "By agent" — same
    // persisted-toggle shape as `scheduledListGroupByAgent` below, just with
    // a third value's worth of choice instead of a boolean.
    homeChannelsGroupBy: HomeChannelsGroupBy;
    setHomeChannelsGroupBy: (groupBy: HomeChannelsGroupBy) => void;
    // Which Channels-section group headers ("channel:slack", "agent:<id>" —
    // namespaced so the two grouping modes' keys can never collide) are
    // expanded to reveal their threads. Same array-not-Set persistence shape
    // as `homeExpandedAgentIds` above, for the same JSON-serializability
    // reason.
    homeExpandedChannelGroupKeys: string[];
    setHomeExpandedChannelGroupKeys: (keys: string[]) => void;
    // "By assignment" (default) vs "By agent" for the Home "Assignments"
    // section — same shape as `homeChannelsGroupBy` above. The section's own
    // collapse state reuses `homeJobsCollapsed`/`setHomeJobsCollapsed`
    // (Assignments lives under the existing Home "Jobs" collapse, not a new
    // flag).
    homeAssignmentsGroupBy: HomeAssignmentsGroupBy;
    setHomeAssignmentsGroupBy: (groupBy: HomeAssignmentsGroupBy) => void;
    // Which Assignments-section group headers ("assignment:<id>",
    // "agent:<id>" — namespaced so the two grouping modes' keys can never
    // collide) are expanded to reveal their threads. Same array-not-Set
    // persistence shape as `homeExpandedChannelGroupKeys` above.
    homeExpandedAssignmentGroupKeys: string[];
    setHomeExpandedAssignmentGroupKeys: (keys: string[]) => void;
    // Workflow ids the user has explicitly starred on the Tasks → Workflows
    // catalog, for quick access to workflows they reach for often. Local/
    // per-device like the kanban filters above — no backend concept of
    // "starred" exists (or is needed) for this.
    starredWorkflowIds: string[];
    toggleStarredWorkflow: (workflowId: string) => void;
    // OS notification preferences. Deliberately device-local (not synced to
    // the backend) — a user running the app on two machines may want
    // notifications on one and not the other.
    notificationsEnabled: boolean;
    setNotificationsEnabled: (enabled: boolean) => void;
    // Show an OS banner/toast when a notification fires.
    notifyBanner: boolean;
    setNotifyBanner: (enabled: boolean) => void;
    // Allowed to play a sound. Whether it *actually* chimes also depends on
    // window-focus tier logic elsewhere — this flag is just the user opt-in.
    notifySound: boolean;
    setNotifySound: (enabled: boolean) => void;
    // Gates only agent-reply notifications (kind "agent.reply") — independent
    // of assignment-fire notifications, which always fire when notifications
    // are enabled.
    notifyAgentReplies: boolean;
    setNotifyAgentReplies: (enabled: boolean) => void;
    // Epoch ms until which notifications are suppressed; null = not snoozed.
    notifySnoozedUntil: number | null;
    snoozeNotifications: (durationMs: number) => void;
    clearNotificationSnooze: () => void;
}

/** Pure helper: true while `now` falls before the store's snooze deadline. */
export function isNotificationSnoozed(
    state: Pick<UserPreferencesState, "notifySnoozedUntil">,
    now: number = Date.now()
): boolean {
    return state.notifySnoozedUntil !== null && now < state.notifySnoozedUntil;
}

export const useUserPreferencesStore = create<UserPreferencesState>()(
    persist(
        (set) => ({
            sidebarWidths: {},
            setSidebarWidthForView: (viewId, width) =>
                set((state) => ({ sidebarWidths: { ...state.sidebarWidths, [viewId]: width } })),
            memoryPanelWidth: 360,
            setMemoryPanelWidth: (width) => set({ memoryPanelWidth: width }),
            channelsColumnWidth: 280,
            setChannelsColumnWidth: (width) => set({ channelsColumnWidth: width }),
            assignmentsColumnWidth: 280,
            setAssignmentsColumnWidth: (width) => set({ assignmentsColumnWidth: width }),
            artifactChatPanelWidth: 320,
            setArtifactChatPanelWidth: (width) => set({ artifactChatPanelWidth: width }),
            memoriesAgentListWidth: 240,
            setMemoriesAgentListWidth: (width) => set({ memoriesAgentListWidth: width }),
            theme: "system",
            setTheme: (theme) => set({ theme }),
            scheduledView: "calendar",
            setScheduledView: (scheduledView) => set({ scheduledView }),
            scheduledListGroupByAgent: true,
            setScheduledListGroupByAgent: (scheduledListGroupByAgent) =>
                set({ scheduledListGroupByAgent }),
            font: "Lato (Default)",
            setFont: (font) => set({ font }),
            bubbleColor: "#1164A3", // Default slack blue-ish or our accent
            setBubbleColor: (bubbleColor) => set({ bubbleColor }),
            timezoneAuto: true,
            setTimezoneAuto: (timezoneAuto) => set({ timezoneAuto }),
            timezone: "America/Los_Angeles",
            setTimezone: (timezone) => set({ timezone }),
            kanbanStatusFilters: [],
            setKanbanStatusFilters: (kanbanStatusFilters) => set({ kanbanStatusFilters }),
            kanbanWorkflowFilters: [],
            setKanbanWorkflowFilters: (kanbanWorkflowFilters) => set({ kanbanWorkflowFilters }),
            appTheme: "default",
            setAppTheme: (appTheme) => set({ appTheme }),
            customThemeColors: null,
            setCustomThemeColors: (customThemeColors) => set({ customThemeColors }),
            circularAvatars: false,
            setCircularAvatars: (circularAvatars) => set({ circularAvatars }),
            showRecentTasks: true,
            setShowRecentTasks: (showRecentTasks) => set({ showRecentTasks }),
            homeJobsCollapsed: false,
            setHomeJobsCollapsed: (homeJobsCollapsed) => set({ homeJobsCollapsed }),
            homeAgentsCollapsed: false,
            setHomeAgentsCollapsed: (homeAgentsCollapsed) => set({ homeAgentsCollapsed }),
            homeExpandedAgentIds: [],
            setHomeExpandedAgentIds: (homeExpandedAgentIds) => set({ homeExpandedAgentIds }),
            homeChannelsCollapsed: false,
            setHomeChannelsCollapsed: (homeChannelsCollapsed) => set({ homeChannelsCollapsed }),
            homeChannelsGroupBy: "channel",
            setHomeChannelsGroupBy: (homeChannelsGroupBy) => set({ homeChannelsGroupBy }),
            homeExpandedChannelGroupKeys: [],
            setHomeExpandedChannelGroupKeys: (homeExpandedChannelGroupKeys) =>
                set({ homeExpandedChannelGroupKeys }),
            homeAssignmentsGroupBy: "assignment",
            setHomeAssignmentsGroupBy: (homeAssignmentsGroupBy) => set({ homeAssignmentsGroupBy }),
            homeExpandedAssignmentGroupKeys: [],
            setHomeExpandedAssignmentGroupKeys: (homeExpandedAssignmentGroupKeys) =>
                set({ homeExpandedAssignmentGroupKeys }),
            starredWorkflowIds: [],
            toggleStarredWorkflow: (workflowId) =>
                set((state) => ({
                    starredWorkflowIds: state.starredWorkflowIds.includes(workflowId)
                        ? state.starredWorkflowIds.filter((id) => id !== workflowId)
                        : [...state.starredWorkflowIds, workflowId],
                })),
            notificationsEnabled: true,
            setNotificationsEnabled: (notificationsEnabled) => set({ notificationsEnabled }),
            notifyBanner: true,
            setNotifyBanner: (notifyBanner) => set({ notifyBanner }),
            notifySound: true,
            setNotifySound: (notifySound) => set({ notifySound }),
            notifyAgentReplies: true,
            setNotifyAgentReplies: (notifyAgentReplies) => set({ notifyAgentReplies }),
            notifySnoozedUntil: null,
            snoozeNotifications: (durationMs) =>
                set({ notifySnoozedUntil: Date.now() + durationMs }),
            clearNotificationSnooze: () => set({ notifySnoozedUntil: null }),
        }),
        {
            name: "user-preferences",
            // No `version`/`migrate` needed: zustand's default merge is a
            // shallow `{...initialState, ...persistedState}`, so fields
            // absent from an older persisted blob (like these notification
            // prefs on first upgrade) fall through to the defaults above
            // rather than landing as `undefined`.
        }
    )
);

/** Reactive hook that returns true when the resolved theme is dark. */
export function useIsDark(): boolean {
    return useSyncExternalStore(
        (cb) => {
            const observer = new MutationObserver(cb);
            observer.observe(document.documentElement, {
                attributes: true,
                attributeFilter: ["data-theme"],
            });
            return () => observer.disconnect();
        },
        () => document.documentElement.getAttribute("data-theme") === "dark",
    );
}
