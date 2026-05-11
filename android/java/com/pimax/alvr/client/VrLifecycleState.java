package com.pimax.alvr.client;

/**
 * Small state machine for VR activity lifecycle and presentation refresh decisions.
 *
 * <p>This keeps the Java-side lifecycle intent free of Android framework dependencies so we can
 * regression-test how resume, focus, and delayed native bootstrap should drive native recovery.
 */
final class VrLifecycleState {
    enum Phase {
        NEW(0),
        CREATED(1),
        RESUMED(2),
        PAUSED(3),
        STOPPED(4),
        DESTROYED(5);

        private final int nativeValue;

        Phase(int nativeValue) {
            this.nativeValue = nativeValue;
        }

        int nativeValue() {
            return nativeValue;
        }
    }

    private Phase phase = Phase.NEW;
    private boolean windowFocused;

    Phase phase() {
        return phase;
    }

    boolean hasWindowFocus() {
        return windowFocused;
    }

    void onCreate() {
        phase = Phase.CREATED;
        windowFocused = false;
    }

    boolean onResume() {
        boolean shouldRefresh = phase != Phase.RESUMED && phase != Phase.DESTROYED;
        phase = Phase.RESUMED;
        return shouldRefresh;
    }

    void onPause() {
        if (phase != Phase.DESTROYED) {
            phase = Phase.PAUSED;
        }
    }

    void onStop() {
        if (phase != Phase.DESTROYED) {
            phase = Phase.STOPPED;
        }
    }

    void onDestroy() {
        phase = Phase.DESTROYED;
        windowFocused = false;
    }

    boolean onWindowFocusChanged(boolean hasFocus) {
        boolean regainedFocus = hasFocus && !windowFocused;
        windowFocused = hasFocus;
        return regainedFocus && phase == Phase.RESUMED;
    }

    boolean shouldRefreshWhenNativeBecomesReady() {
        return phase == Phase.RESUMED;
    }
}
