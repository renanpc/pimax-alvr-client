package com.pimax.alvr.client;

/**
 * Small state machine for Pimax proximity-driven display wake/sleep handling.
 *
 * <p>These transitions are intentionally kept free of Android framework dependencies so the
 * black-screen wake behavior can be unit-tested. This is important for the headset sleep feature:
 * a single noisy "far" sample must not blank the display while the headset is being worn, but a
 * stable off-head state still needs to release the app wake lock so Pimax can sleep normally.
 */
final class ProximityWakePolicy {
    /** Startup grace before an absent proximity sample is treated as off-head. */
    static final long UNKNOWN_SLEEP_GRACE_MS = 10_000L;

    /** Far samples must remain stable before the app lets Pimax put the panel to sleep. */
    static final long FAR_SLEEP_GRACE_MS = 8_000L;

    enum Action {
        IGNORE,
        NEAR,
        FAR_PENDING,
        FAR_STABLE,
        UNKNOWN_FAR
    }

    private boolean headsetNear;
    private boolean proximityStateKnown;
    private boolean proximityFarPending;
    private boolean paused;

    void setPaused(boolean paused) {
        this.paused = paused;
    }

    boolean isPaused() {
        return paused;
    }

    boolean isHeadsetNear() {
        return headsetNear;
    }

    boolean isProximityStateKnown() {
        return proximityStateKnown;
    }

    boolean isProximityFarPending() {
        return proximityFarPending;
    }

    boolean shouldKeepDisplayAwakeForProximity() {
        return !proximityStateKnown || headsetNear;
    }

    Action onUnknownSleepGraceElapsed() {
        if (proximityStateKnown || paused) {
            return Action.IGNORE;
        }
        proximityStateKnown = true;
        headsetNear = false;
        proximityFarPending = false;
        return Action.UNKNOWN_FAR;
    }

    Action onProximitySample(boolean isNear) {
        proximityStateKnown = true;
        if (isNear) {
            headsetNear = true;
            proximityFarPending = false;
            return Action.NEAR;
        }

        if (paused) {
            return Action.IGNORE;
        }

        proximityFarPending = true;
        return Action.FAR_PENDING;
    }

    Action onFarSleepGraceElapsed() {
        if (!proximityFarPending || paused) {
            return Action.IGNORE;
        }
        proximityFarPending = false;
        headsetNear = false;
        proximityStateKnown = true;
        return Action.FAR_STABLE;
    }

    boolean cancelPendingFar() {
        boolean wasPending = proximityFarPending;
        proximityFarPending = false;
        return wasPending;
    }
}
