/**
 * Tests for the notification-prefs slice of userPreferencesStore.ts: default
 * values must be permissive (notifications on) out of the box, snoozing must
 * set/clear `notifySnoozedUntil` correctly, and `isNotificationSnoozed` must
 * treat the snooze deadline as an exclusive upper bound (still snoozed right
 * up to the boundary, no longer snoozed once `now` reaches it).
 */

import { describe, it, expect, beforeEach } from "vitest";
import {
  useUserPreferencesStore,
  isNotificationSnoozed,
} from "../userPreferencesStore";

function store() {
  return useUserPreferencesStore.getState();
}

const NOTIFICATION_DEFAULTS = {
  notificationsEnabled: true,
  notifyBanner: true,
  notifySound: true,
  notifySnoozedUntil: null as number | null,
};

beforeEach(() => {
  useUserPreferencesStore.setState(NOTIFICATION_DEFAULTS);
});

describe("userPreferencesStore notification prefs", () => {
  it("defaults to notifications on, banner on, sound on, no snooze", () => {
    expect(store().notificationsEnabled).toBe(true);
    expect(store().notifyBanner).toBe(true);
    expect(store().notifySound).toBe(true);
    expect(store().notifySnoozedUntil).toBeNull();
  });

  it("setNotificationsEnabled/setNotifyBanner/setNotifySound toggle independently", () => {
    store().setNotificationsEnabled(false);
    expect(store().notificationsEnabled).toBe(false);
    expect(store().notifyBanner).toBe(true);
    expect(store().notifySound).toBe(true);

    store().setNotifyBanner(false);
    expect(store().notifyBanner).toBe(false);
    expect(store().notifySound).toBe(true);

    store().setNotifySound(false);
    expect(store().notifySound).toBe(false);
  });

  it("snoozeNotifications sets a future epoch-ms deadline; clearNotificationSnooze resets it", () => {
    const before = Date.now();
    store().snoozeNotifications(60_000);
    const deadline = store().notifySnoozedUntil;

    expect(deadline).not.toBeNull();
    expect(deadline as number).toBeGreaterThanOrEqual(before + 60_000);

    store().clearNotificationSnooze();
    expect(store().notifySnoozedUntil).toBeNull();
  });

  it("isNotificationSnoozed is false when there is no snooze set", () => {
    expect(isNotificationSnoozed(store(), Date.now())).toBe(false);
  });

  it("isNotificationSnoozed is true strictly before the deadline and false at/after it", () => {
    const state = { notifySnoozedUntil: 1_000 };

    // Strictly before the boundary: still snoozed.
    expect(isNotificationSnoozed(state, 999)).toBe(true);
    // Exactly at the boundary: no longer snoozed (exclusive upper bound).
    expect(isNotificationSnoozed(state, 1_000)).toBe(false);
    // Past the boundary: no longer snoozed.
    expect(isNotificationSnoozed(state, 1_001)).toBe(false);
  });
});
