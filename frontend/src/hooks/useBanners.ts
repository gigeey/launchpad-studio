import { useEffect } from "react";
import { useBannerStore } from "../stores/bannerStore";
import { useNetworkStore } from "../stores/networkStore";
import { getPreferencesStatus } from "../lib/api";

const PREFS_ALERT_DISMISSED_KEY = "preferencesAlertDismissed";

/**
 * Registers global banners (offline, preferences, etc.)
 * Call once in AppShell.
 */
export function useBanners(callbacks?: {
  onOpenSettings?: () => void;
}) {
  const addBanner = useBannerStore((s) => s.addBanner);
  const removeBanner = useBannerStore((s) => s.removeBanner);
  const dismissBanner = useBannerStore((s) => s.dismissBanner);

  // --- Internet offline banner ---
  const isInternetOnline = useNetworkStore((s) => s.isInternetOnline);

  useEffect(() => {
    if (!isInternetOnline) {
      addBanner({
        id: "internet-offline",
        priority: 100,
        message:
          "No internet connection. Some features may not work until connectivity is restored.",
        variant: "error",
        dismissible: false,
      });
    } else {
      removeBanner("internet-offline");
    }
  }, [isInternetOnline, addBanner, removeBanner]);

  // --- Server offline banner ---
  const isServerOnline = useNetworkStore((s) => s.isServerOnline);

  useEffect(() => {
    if (!isServerOnline) {
      addBanner({
        id: "server-offline",
        priority: 90,
        message:
          "Unable to reach the server. The backend may be down or restarting.",
        variant: "warning",
        dismissible: false,
      });
    } else {
      removeBanner("server-offline");
    }
  }, [isServerOnline, addBanner, removeBanner]);

  // --- Preferences banner ---
  useEffect(() => {
    if (sessionStorage.getItem(PREFS_ALERT_DISMISSED_KEY)) return;

    getPreferencesStatus()
      .then((status) => {
        if (!status.configured) {
          addBanner({
            id: "preferences",
            priority: 10,
            message:
              "Your profile is not set up yet. Go to Settings to configure your name and preferences.",
            variant: "warning",
            dismissible: true,
            action: callbacks?.onOpenSettings
              ? { label: "Go to Settings", onClick: callbacks.onOpenSettings }
              : undefined,
          });
        }
      })
      .catch(() => {
        // Silently ignore
      });
  }, [addBanner, callbacks?.onOpenSettings]);

  // Sync dismissal to sessionStorage for preferences
  useEffect(() => {
    return useBannerStore.subscribe((state, prev) => {
      if (state.dismissed.has("preferences") && !prev.dismissed.has("preferences")) {
        sessionStorage.setItem(PREFS_ALERT_DISMISSED_KEY, "1");
      }
    });
  }, []);

  return { dismissBanner };
}
