package com.pimax.alvr.client;

/**
 * Regression tests for the black-screen wake policy.
 *
 * <p>These tests are important for the Pimax sleep/wake feature because regressions here can
 * make the headset keep streaming to a black panel after it is put back on the user's face.
 */
public final class ProximityWakePolicyTest {
    public static void main(String[] args) {
        nearImmediatelyKeepsDisplayAwake();
        farRequiresStableGraceBeforeSleep();
        nearCancelsPendingFarBeforeSleep();
        pausedFarSampleIsIgnored();
        pausedFarConfirmationIsIgnored();
        unknownStartupGraceAllowsOffHeadSleep();
        System.out.println("ProximityWakePolicyTest passed");
    }

    private static void nearImmediatelyKeepsDisplayAwake() {
        ProximityWakePolicy policy = new ProximityWakePolicy();

        assertEquals(
                "near action",
                ProximityWakePolicy.Action.NEAR,
                policy.onProximitySample(true));
        assertTrue("near marks proximity known", policy.isProximityStateKnown());
        assertTrue("near marks headset worn", policy.isHeadsetNear());
        assertTrue("near keeps display awake", policy.shouldKeepDisplayAwakeForProximity());
        assertFalse("near has no pending far confirmation", policy.isProximityFarPending());
    }

    private static void farRequiresStableGraceBeforeSleep() {
        ProximityWakePolicy policy = new ProximityWakePolicy();
        policy.onProximitySample(true);

        assertEquals(
                "far schedules confirmation",
                ProximityWakePolicy.Action.FAR_PENDING,
                policy.onProximitySample(false));
        assertTrue("far confirmation is pending", policy.isProximityFarPending());
        assertTrue("single far sample does not mark headset off-head", policy.isHeadsetNear());
        assertTrue(
                "single far sample still keeps display awake",
                policy.shouldKeepDisplayAwakeForProximity());

        assertEquals(
                "stable far confirms sleep",
                ProximityWakePolicy.Action.FAR_STABLE,
                policy.onFarSleepGraceElapsed());
        assertFalse("stable far clears pending confirmation", policy.isProximityFarPending());
        assertFalse("stable far marks headset off-head", policy.isHeadsetNear());
        assertFalse(
                "stable far allows display sleep",
                policy.shouldKeepDisplayAwakeForProximity());
    }

    private static void nearCancelsPendingFarBeforeSleep() {
        ProximityWakePolicy policy = new ProximityWakePolicy();
        policy.onProximitySample(true);
        policy.onProximitySample(false);

        assertEquals(
                "near cancels pending far",
                ProximityWakePolicy.Action.NEAR,
                policy.onProximitySample(true));
        assertFalse("far confirmation was cancelled", policy.isProximityFarPending());
        assertEquals(
                "cancelled far confirmation is ignored",
                ProximityWakePolicy.Action.IGNORE,
                policy.onFarSleepGraceElapsed());
        assertTrue("headset remains worn after cancelled far", policy.isHeadsetNear());
    }

    private static void pausedFarConfirmationIsIgnored() {
        ProximityWakePolicy policy = new ProximityWakePolicy();
        policy.onProximitySample(true);
        policy.onProximitySample(false);
        policy.setPaused(true);

        assertEquals(
                "paused confirmation ignored",
                ProximityWakePolicy.Action.IGNORE,
                policy.onFarSleepGraceElapsed());
        assertTrue("paused ignored confirmation remains pending", policy.isProximityFarPending());
        assertTrue("paused ignored confirmation keeps headset near", policy.isHeadsetNear());
    }

    private static void pausedFarSampleIsIgnored() {
        ProximityWakePolicy policy = new ProximityWakePolicy();
        policy.onProximitySample(true);
        policy.setPaused(true);

        assertEquals(
                "paused far sample ignored",
                ProximityWakePolicy.Action.IGNORE,
                policy.onProximitySample(false));
        assertFalse("paused far sample does not schedule confirmation", policy.isProximityFarPending());
        assertTrue("paused far sample keeps headset near", policy.isHeadsetNear());
    }

    private static void unknownStartupGraceAllowsOffHeadSleep() {
        ProximityWakePolicy policy = new ProximityWakePolicy();

        assertTrue(
                "unknown startup state keeps display awake initially",
                policy.shouldKeepDisplayAwakeForProximity());
        assertEquals(
                "unknown grace assumes off-head",
                ProximityWakePolicy.Action.UNKNOWN_FAR,
                policy.onUnknownSleepGraceElapsed());
        assertTrue("unknown grace marks proximity known", policy.isProximityStateKnown());
        assertFalse("unknown grace marks headset off-head", policy.isHeadsetNear());
        assertFalse(
                "unknown grace allows display sleep",
                policy.shouldKeepDisplayAwakeForProximity());
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
