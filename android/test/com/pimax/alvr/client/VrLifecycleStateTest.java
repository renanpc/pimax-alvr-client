package com.pimax.alvr.client;

/**
 * Regression tests for VR activity lifecycle recovery decisions.
 */
public final class VrLifecycleStateTest {
    public static void main(String[] args) {
        resumeRequestsRefreshAfterCreate();
        focusGainRequestsRefreshOnlyWhileResumed();
        pausedAndStoppedTransitionsBlockFocusRefresh();
        nativeReadyRefreshRequiresResumedState();
        destroyStopsFurtherRefreshRequests();
        System.out.println("VrLifecycleStateTest passed");
    }

    private static void resumeRequestsRefreshAfterCreate() {
        VrLifecycleState state = new VrLifecycleState();

        state.onCreate();
        assertEquals("create phase", VrLifecycleState.Phase.CREATED, state.phase());
        assertTrue("resume after create requests refresh", state.onResume());
        assertEquals("resume phase", VrLifecycleState.Phase.RESUMED, state.phase());
        assertFalse("duplicate resume does not request refresh", state.onResume());
    }

    private static void focusGainRequestsRefreshOnlyWhileResumed() {
        VrLifecycleState state = new VrLifecycleState();
        state.onCreate();
        state.onResume();

        assertTrue("first focus gain while resumed refreshes", state.onWindowFocusChanged(true));
        assertFalse("duplicate focus gain while already focused does not refresh", state.onWindowFocusChanged(true));
        assertFalse("focus loss does not refresh", state.onWindowFocusChanged(false));
        assertTrue("regained focus while resumed refreshes", state.onWindowFocusChanged(true));
    }

    private static void pausedAndStoppedTransitionsBlockFocusRefresh() {
        VrLifecycleState state = new VrLifecycleState();
        state.onCreate();
        state.onResume();
        state.onWindowFocusChanged(true);
        state.onPause();

        assertFalse("focus gain while paused does not refresh", state.onWindowFocusChanged(true));
        state.onStop();
        assertFalse("focus gain while stopped does not refresh", state.onWindowFocusChanged(true));
    }

    private static void nativeReadyRefreshRequiresResumedState() {
        VrLifecycleState state = new VrLifecycleState();
        state.onCreate();
        assertFalse("native-ready does not refresh before resume", state.shouldRefreshWhenNativeBecomesReady());
        state.onResume();
        assertTrue("native-ready refreshes while resumed", state.shouldRefreshWhenNativeBecomesReady());
        state.onPause();
        assertFalse("native-ready does not refresh while paused", state.shouldRefreshWhenNativeBecomesReady());
    }

    private static void destroyStopsFurtherRefreshRequests() {
        VrLifecycleState state = new VrLifecycleState();
        state.onCreate();
        state.onResume();
        state.onDestroy();

        assertEquals("destroy phase", VrLifecycleState.Phase.DESTROYED, state.phase());
        assertFalse("native-ready does not refresh after destroy", state.shouldRefreshWhenNativeBecomesReady());
        assertFalse("focus gain does not refresh after destroy", state.onWindowFocusChanged(true));
        assertFalse("resume after destroy does not refresh", state.onResume());
    }

    private static void assertTrue(String message, boolean value) {
        if (!value) {
            throw new AssertionError(message);
        }
    }

    private static void assertFalse(String message, boolean value) {
        if (value) {
            throw new AssertionError(message);
        }
    }

    private static void assertEquals(String message, Object expected, Object actual) {
        if (!expected.equals(actual)) {
            throw new AssertionError(message + ": expected " + expected + ", got " + actual);
        }
    }
}
