package com.pimax.alvr.client;

import android.app.NativeActivity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Build;
import android.os.Binder;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.Parcel;
import android.os.PowerManager;
import android.os.SystemClock;
import android.os.RemoteException;
import android.provider.Settings;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.util.Log;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;
import java.io.BufferedReader;
import java.io.File;
import java.io.FileReader;
import java.io.IOException;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

/**
 * Main VR activity for the Pimax ALVR client.
 *
 * <p>This activity extends {@link NativeActivity} to host the ALVR native render loop. It bridges
 * Android system events (screen state, proximity, Pimax hardware IPD changes) to the native layer
 * via JNI callbacks, and manages wake locks to keep the device and render loop alive during use.
 *
 * <p>Key responsibilities:
 * <ul>
 *   <li>Maintain a {@link PowerManager.WakeLock} so the display and native render loop stay alive
 *       even when the activity is paused/stopped (Pimax XR entry can pause the activity while
 *       rendering continues).</li>
 *   <li>Sync IPD (interpupillary distance) changes from the Pimax hardware bridge to the native
 *       layer so镜片间距 can be adjusted dynamically.</li>
 *   <li>Forward proximity sensor events to the native layer to detect when the headset is put on
 *       or taken off.</li>
 *   <li>Handle exit key combinations (Back, Escape, Menu, Select, Start) to cleanly shut down
 *       the activity and native render loop.</li>
 *   <li>Broadcast a shutdown intent when the system or user requests termination.</li>
 *   <li>Toggle the peak display refresh rate (90 Hz on resume, 72 Hz on pause) to balance
 *       performance and power consumption.</li>
 * </ul>
 *
 * <p>Lifecycle summary:
 * <ul>
 *   <li>{@link #onCreate}: Acquires wake lock, registers screen/proximity/Pimax hardware
 *       receivers, notifies native of screen-on.</li>
 *   <li>{@link #onResume}: Resets shutdown flag, sets peak refresh rate to 90 Hz, re-registers
 *       bridges if needed.</li>
 *   <li>{@link #onPause}/{@ #onStop}: Reduces refresh rate to 72 Hz but intentionally keeps the
 *       wake lock and render loop alive — Pimax XR entry can pause the activity while rendering
 *       continues on the native side.</li>
 *   <li>{@link #onDestroy}: Requests native shutdown, unregisters all sensors and receivers,
 *       releases wake lock.</li>
 * </ul>
 */
public final class VrRenderActivity extends NativeActivity {

    // =========================================================================================
    // Constants
    // =========================================================================================

    /** Logging tag for this class. */
    private static final String TAG = "PimaxALVRActivity";

    /**
     * Window flags applied in {@link #onCreate} — full-screen, high-performance mode.
     * {@code 132} decodes to {@code FLAG_FULL_SCREEN | FLAG_HIDE_NAVIGATION |
     * FLAG_KEEP_SCREEN_ON | FLAG_ALLOW_LOCK_WHILE_SCREEN_ON}.
     */
    private static final int WINDOW_FLAGS_ON_CREATE = 132;

    /**
     * Window flags applied in {@link #onResume} and on window focus gain.
     * {@code 128} decodes to {@code FLAG_KEEP_SCREEN_ON | FLAG_ALLOW_LOCK_WHILE_SCREEN_ON}.
     */
    private static final int WINDOW_FLAGS_ON_FOCUS = 128;

    /**
     * System UI visibility flags applied when the activity gains window focus — immersive
     * sticky mode with layout fullscreen.
     */
    private static final int SYSTEM_UI_VISIBILITY_FLAGS = 5894;

    /**
     * Timeout for the activity wake lock in milliseconds. Default 6 hours.
     *
     * <p>The render loop is meant to run for long sessions, so a short timeout can
     * let the display dim or sleep even while streaming is still active. We keep a
     * large timeout as a safety net and also release explicitly during cleanup.
     */
    private static final long WAKE_LOCK_TIMEOUT_MS = 6 * 60 * 60 * 1000L;

    /**
     * Custom broadcast action sent by this activity when it receives a shutdown request
     * (e.g., user exit key). Other components can register to receive this to perform
     * cleanup.
     */
    private static final String ACTION_SHUTDOWN = "com.pimax.alvr.client.ACTION_SHUTDOWN";

    /** Pimax vendor setting used by stock WiFi Airlink's HmdSync while streaming. */
    private static final String PIMAX_EYECHIP_ON_SETTING = "eyechip_on";

    // ---- Pimax hardware bridge constants ---------------------------------------------

    /**
     * Fully-qualified class name for the Pimax hardware event listener interface.
     * Used with reflection to register for IPD and lens-change events without a
     * direct compile-time dependency on the Pimax framework.
     */
    private static final String PMX_HW_LISTENER_DESCRIPTOR = "android.app.pmx.IPmxHwEventListener";

    /** Event type code for motor movement (IPD adjustment knob turns). */
    private static final int PMX_EVENT_TYPE_MOTOR = 1;

    /** Event type code for lens module changed (different prescription lens inserted). */
    private static final int PMX_EVENT_TYPE_LENS_CHANGED = 2;

    /**
     * Combined event type mask requesting both motor and lens-changed events from the
     * Pimax hardware manager.
     */
    private static final int PMX_EVENT_TYPE_MOTOR_AND_LENS = 3;

    /**
     * Loading {@code libpxrapi.so} explicitly can stall startup on some headset builds even
     * though the framework already has a usable copy available. Keep this off unless we need
     * to force the APK-bundled library for debugging.
     */
    private static final boolean LOAD_PXRAPI_EAGERLY = false;

    // =========================================================================================
    // Fields
    // =========================================================================================

    // ---- Wake lock -----------------------------------------------------------------

    /** Wake lock to keep the CPU and display on throughout the VR session. */
    private PowerManager.WakeLock screenWakeLock;

    /** Tracks whether {@link #screenReceiver} has been registered to avoid duplicate registration. */
    private boolean screenReceiverRegistered;

    /** Main-thread handler used to retry waking the display when the headset is put back on. */
    private Handler screenWakeHandler;

    /** True while the proximity sensor reports that the headset is near the face. */
    private boolean headsetNear;

    /** Latest screen-on/off state that should be forwarded to native when it becomes ready. */
    private boolean pendingNativeScreenOn = true;

    /** Counts the retries for the current near-face wake sequence. */
    private int screenWakeRetryCount;

    /** Avoids repeatedly logging vendor setting writes that Android has already denied. */
    private boolean pimaxEyechipWriteDenied;

    /** Avoids repeatedly logging refresh-rate writes that Android has already denied. */
    private boolean peakRefreshRateWriteDenied;

    /** Hidden PowerManager.wakeUp requires DEVICE_POWER on this headset build. */
    private boolean powerWakeUpDenied;

    /** Re-pokes display power until the panel reports itself interactive again. */
    private final Runnable screenWakeRetryRunnable = new Runnable() {
        @Override
        public void run() {
            retryDisplayWakeIfNeeded();
        }
    };

    // ---- Activity state ------------------------------------------------------------

    /** Set to true when the activity is paused/stopped; guards against resume after destroy. */
    private boolean paused;

    /**
     * Set to true when the native layer has requested shutdown. Once set the activity
     * refuses to resume (finishes instead) to prevent a stale render loop from running.
     */
    private boolean nativeShutdownRequested;

    /** Guards the async native bootstrap so the activity can reach onCreate quickly. */
    private final Object nativeBootstrapLock = new Object();

    /** True once the native bootstrap thread has been started. */
    private volatile boolean nativeBootstrapStarted;

    /** True once both native libraries have loaded successfully. */
    private volatile boolean nativeLibrariesLoaded;

    // ---- Pimax hardware bridge (IPD sync) -----------------------------------------

    /**
     * Reference to the Pimax hardware manager service, obtained via
     * {@code getSystemService("pmx_hw")}. Stored so it can be used to unregister the
     * listener in {@link #unregisterPimaxHardwareBridge}.
     */
    private Object pmxHwManager;

    /**
     * A dynamic proxy implementing {@code IPmxHwEventListener} that dispatches incoming
     * hardware events to {@link #onPimaxHwEvent}. Created using {@link Proxy#newProxyInstance}.
     */
    private Object pmxHwListenerProxy;

    /**
     * The {@code IPmxHwEventListener} interface class, looked up via {@code Class.forName}
     * at registration time. Stored so it can be passed to the unregister method.
     */
    private Class<?> pmxHwListenerClass;

    /**
     * The {@link Binder} that serves as the real {@code IPmxHwEventListener} backend.
     * The proxy's {@code asBinder()} returns this binder so the system can hold a reference
     * to the listener independently of the proxy object.
     */
    private Binder pmxHwCallbackBinder;

    /** Tracks whether the Pimax hardware listener has been successfully registered. */
    private boolean pmxHwRegistered;

    // ---- Proximity sensor ----------------------------------------------------------

    /** System sensor manager, lazily obtained in {@link #registerProximitySensor}. */
    private SensorManager sensorManager;

    /** The default proximity sensor on the device. Null if unavailable. */
    private Sensor proximitySensor;

    /** Tracks whether the proximity sensor listener has been registered. */
    private boolean proximityRegistered;

    /**
     * Listener that receives proximity sensor events and forwards them to the native
     * layer as a boolean (near / not near).
     *
     * <p>The proximity sensor detects when the headset is brought close to the user's face.
     * When near, the native layer can pause video decoding or reduce render quality to
     * conserve power; when far, it can resume full-quality rendering.
     */
    private final SensorEventListener proximityListener = new SensorEventListener() {
        /**
         * Called when the proximity sensor detects a change.
         *
         * @param event the sensor event; {@code event.values[0]} holds the raw distance value.
         */
        @Override
        public void onSensorChanged(SensorEvent event) {
            // The proximity sensor typically returns 0 (near) or 5+ cm (far) on Pimax devices.
            float distance = event != null && event.values.length > 0 ? event.values[0] : Float.NaN;
            boolean isNear = distance < 1.0f;
            headsetNear = isNear;
            if (nativeLibrariesLoaded) {
                nativeNotifyProximity(isNear);
            }
            if (isNear) {
                runOnUiThread(() -> handleHeadsetNear("proximity near"));
            } else {
                runOnUiThread(() -> handleHeadsetFar("proximity far"));
            }
        }

        @Override
        public void onAccuracyChanged(Sensor sensor, int accuracy) {
            // Not used — proximity changes are binary (near/far), accuracy is irrelevant.
        }
    };

    // =========================================================================================
    // Broadcast receiver — screen state and shutdown
    // =========================================================================================

    /**
     * Receives system broadcasts for screen-on, screen-off, and the custom ALVR shutdown action.
     *
     * <p>Screen broadcasts are used to keep the native render loop informed about display state:
     * when the screen turns off the native layer may reduce its render rate or pause certain
     * subsystems; when it turns back on it resumes full operation.
     *
     * <p>The shutdown broadcast ({@link #ACTION_SHUTDOWN}) is sent by this activity itself
     * when the user presses an exit key — other components can register to receive it and
     * perform their own cleanup.
     */
    private final BroadcastReceiver screenReceiver = new BroadcastReceiver() {
        /**
         * Dispatches screen lifecycle events to the native layer and responds to shutdown requests.
         *
         * @param context the context (unused)
         * @param intent  the broadcast intent; may be null in rare system scenarios
         */
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent != null ? intent.getAction() : null;
            Log.i(TAG, "screenReceiver.onReceive(" + action + ")");
            if (Intent.ACTION_SCREEN_ON.equals(action)) {
                // Screen has turned on — notify native so it can resume rendering.
                pendingNativeScreenOn = true;
                setPimaxEyechipEnabled(false, "screen-on broadcast");
                if (nativeLibrariesLoaded) {
                    nativeNotifyScreen(true);
                }
                pokeDisplayPower("screen-on broadcast");
                forceScreenWakeLock("screen-on broadcast");
            } else if (Intent.ACTION_SCREEN_OFF.equals(action)) {
                // Screen has turned off — notify native but intentionally keep the app
                // running. Re-acquire the wake lock immediately so a transient screen-off
                // event does not leave the headset blank while streaming continues.
                pendingNativeScreenOn = false;
                setPimaxEyechipEnabled(false, "screen-off broadcast");
                if (nativeLibrariesLoaded) {
                    nativeNotifyScreen(false);
                }
                forceScreenWakeLock("screen-off broadcast");
                if (headsetNear) {
                    startDisplayWakeRetry("screen-off broadcast while near");
                }
                Log.i(TAG, "screen turned off; keeping app running for development");
            } else if (ACTION_SHUTDOWN.equals(action)) {
                Log.i(TAG, "received ALVR shutdown broadcast");
                shutdownAndFinish("shutdown broadcast");
            }
        }
    };

    /**
     * Starts loading the native libraries on a background thread so the activity can
     * finish its Java lifecycle setup before the Pimax JNI bootstrap runs.
     */
    private void startNativeBootstrapIfNeeded(String reason) {
        synchronized (nativeBootstrapLock) {
            if (nativeBootstrapStarted) {
                return;
            }
            nativeBootstrapStarted = true;
        }
        Log.i(TAG, "starting native bootstrap asynchronously: " + reason);
        Thread bootstrapThread = new Thread(() -> {
            try {
                if (LOAD_PXRAPI_EAGERLY) {
                    try {
                        System.loadLibrary("pxrapi");
                        Log.i(TAG, "loaded pxrapi");
                    } catch (UnsatisfiedLinkError error) {
                        Log.w(TAG, "pxrapi library is not in this APK path; continuing with framework-loaded PxrApi", error);
                    }
                }
                System.loadLibrary("pimax_alvr_client");
                Log.i(TAG, "loaded pimax_alvr_client");
                nativeLibrariesLoaded = true;
                runOnUiThread(() -> flushDeferredNativeState("native bootstrap complete"));
            } catch (UnsatisfiedLinkError error) {
                Log.w(TAG, "failed to load native libraries", error);
            } catch (RuntimeException error) {
                Log.w(TAG, "native bootstrap failed", error);
            }
        }, "PimaxNativeBootstrap");
        bootstrapThread.setDaemon(true);
        bootstrapThread.start();
    }

    /**
     * Replays the latest screen/proximity state once native libraries are available.
     */
    private void flushDeferredNativeState(String reason) {
        if (!nativeLibrariesLoaded) {
            return;
        }
        Log.i(TAG, "flushing deferred native state: " + reason
                + " screenOn=" + pendingNativeScreenOn
                + " headsetNear=" + headsetNear);
        resetNativeShutdown(reason);
        nativeNotifyScreen(pendingNativeScreenOn);
        nativeNotifyProximity(headsetNear);
        registerPimaxHardwareBridge();
        registerProximitySensor();
        startControllerPoller();
        if (headsetNear) {
            startDisplayWakeRetry("native bootstrap complete");
        }
    }

    /**
     * Reasserts the display wake path on the UI thread when the headset is put back on.
     */
    private void handleHeadsetNear(String reason) {
        pendingNativeScreenOn = true;
        setPimaxEyechipEnabled(false, reason);
        if (nativeLibrariesLoaded) {
            nativeNotifyScreen(true);
        }
        getWindow().addFlags(WINDOW_FLAGS_ON_CREATE | WINDOW_FLAGS_ON_FOCUS);
        pokeDisplayPower(reason);
        forceScreenWakeLock(reason);
        startDisplayWakeRetry(reason);
    }

    /** Stops the wake retry loop when the headset is removed again. */
    private void handleHeadsetFar(String reason) {
        setPimaxEyechipEnabled(false, reason);
        // Proximity-far means "not currently worn", not "the Android display is off".
        // Keep the last real screen state intact so launching off-head does not flush
        // screenOn=false into the native Pimax renderer and leave presentation black.
        stopDisplayWakeRetry(reason);
    }

    // =========================================================================================
    // JNI native method declarations
    // =========================================================================================

    /**
     * Notifies the native layer that the activity is about to finish and the render loop
     * should shut down gracefully. The native side is expected to release all OpenGL / video
     * resources and terminate its render thread.
     */
    private static native void nativeRequestShutdown();

    /**
     * Clears the native shutdown flag. Called when the activity resumes to allow a fresh
     * render session after a previous shutdown was requested.
     */
    private static native void nativeResetShutdown();

    /**
     * Sends an updated IPD (interpupillary distance) value from the Pimax hardware bridge
     * to the native layer. The native layer uses this to adjust the per-eye projection
     * matrices and keep the rendered image aligned with the user's pupils.
     *
     * @param rawIpd the raw IPD value in millimeters, as reported by the Pimax motor sensor
     */
    private static native void nativeNotifyIpdChange(float rawIpd);

    /**
     * Notifies the native layer whether the proximity sensor detects the headset is near
     * the user's face (near = true) or away (far = false).
     *
     * @param isNear true when the headset is being worn, false when it has been removed
     */
    private static native void nativeNotifyProximity(boolean isNear);

    /**
     * Notifies the native layer of display screen state changes.
     *
     * @param isScreenOn true if the display is currently on, false if it is off
     */
    private static native void nativeNotifyScreen(boolean isScreenOn);

    /**
     * Pushes a controller state snapshot to the native layer.
     *
     * @param hand 0 = left, 1 = right
     * @param handle opaque handle (currently unused, reserved for shared-mem path)
     * @param buttonsPressed bitmask — see {@code controller.rs} for bit layout
     * @param buttonsTouched bitmask of capacitive touches (0 if hardware lacks support)
     * @param trigger analog trigger value [0.0, 1.0]
     * @param grip analog grip value [0.0, 1.0]
     * @param thumbstickX thumbstick X axis [-1.0, 1.0]
     * @param thumbstickY thumbstick Y axis [-1.0, 1.0]
     * @param battery battery percentage [0, 100]
     */
    private static native void nativeNotifyControllerState(
            int hand, int handle,
            int buttonsPressed, int buttonsTouched,
            float trigger, float grip,
            float thumbstickX, float thumbstickY,
            int battery);

    /**
     * Notifies the native layer that a controller has connected or disconnected.
     *
     * @param hand 0 = left, 1 = right
     * @param connected true on connect, false on disconnect
     */
    private static native void nativeNotifyControllerConnection(int hand, boolean connected);

    // =========================================================================================
    // Activity lifecycle
    // =========================================================================================

    /**
     * Called when the activity is first created.
     *
     * <p>Initializes the activity:
     * <ul>
     *   <li>Applies full-screen window flags for immersive VR.</li>
     *   <li>Creates and acquires a wake lock so the device stays on throughout the session.</li>
     *   <li>Registers for screen-on/off and shutdown broadcasts.</li>
     *   <li>Registers the Pimax hardware bridge to receive IPD and lens-change events.</li>
     *   <li>Registers the proximity sensor to detect headset proximity changes.</li>
     *   <li>Notifies the native layer that the screen is on.</li>
     * </ul>
     *
     * @param savedInstanceState if the activity is being re-created from a saved state,
     *                           this bundle contains the previously saved state (not used here)
     */
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "VrRenderActivity.onCreate");
        paused = false;
        nativeShutdownRequested = false;
        pendingNativeScreenOn = true;
        // Clear any stale native shutdown flag from a previous session.
        resetNativeShutdown("onCreate");
        // Apply full-screen, high-keep-awake flags for VR.
        getWindow().addFlags(WINDOW_FLAGS_ON_CREATE);
        createScreenWakeLock();
        screenWakeHandler = new Handler(Looper.getMainLooper());
        setPimaxEyechipEnabled(false, "onCreate active streaming");
        registerScreenReceiver();
        acquireScreenWakeLock("onCreate");
        registerPimaxHardwareBridge();
        registerProximitySensor();
        startControllerPoller();
        startNativeBootstrapIfNeeded("onCreate");
    }

    /**
     * Called when the activity comes to the foreground.
     *
     * <p>Resumes the VR session:
     * <ul>
     *   <li>Checks if native shutdown was already requested — if so, finishes immediately
     *       to avoid resuming a stale render loop.</li>
     *   <li>Clears the native shutdown flag so the render loop can run again.</li>
     *   <li>Sets the peak display refresh rate to 90 Hz for smooth head tracking.</li>
     *   <li>Applies window flags and re-acquires the wake lock.</li>
     *   <li>Re-registers screen, Pimax hardware bridge, and proximity sensor if needed.</li>
     * </ul>
     */
    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "VrRenderActivity.onResume");
        paused = false;
        // Guard against resuming after a shutdown request — a stale render loop should
        // not run as it may produce garbled visuals or crash.
        if (nativeShutdownRequested) {
            Log.i(TAG, "native shutdown already requested; finishing instead of resuming");
            finishActivity("resume after native shutdown");
            return;
        }
        resetNativeShutdown("onResume");
        // 90 Hz provides the smoothest head tracking on Pimax Crystal.
        trySetPeakRefreshRate(90.0f, "onResume");
        setPimaxEyechipEnabled(false, "onResume active streaming");
        getWindow().addFlags(WINDOW_FLAGS_ON_CREATE | WINDOW_FLAGS_ON_FOCUS);
        registerScreenReceiver();
        pendingNativeScreenOn = true;
        if (nativeLibrariesLoaded) {
            nativeNotifyScreen(true);
        }
        pokeDisplayPower("onResume");
        forceScreenWakeLock("onResume");
        if (headsetNear) {
            startDisplayWakeRetry("onResume while near");
        }
        registerPimaxHardwareBridge();
        registerProximitySensor();
        startControllerPoller();
        startNativeBootstrapIfNeeded("onResume");
    }

    /**
     * Called when the activity is leaving the foreground.
     *
     * <p>Pauses the activity but intentionally keeps the native render loop and wake lock
     * alive. This is because Pimax's XR runtime can pause the activity (via {@link #onPause}
     * / {@link #onStop}) while still running the render loop in the native layer — shutting
     * down the render loop here would break the VR session.
     *
     * <p>The peak refresh rate is reduced to 72 Hz to conserve power during pauses.
     */
    @Override
    protected void onPause() {
        Log.i(TAG, "VrRenderActivity.onPause");
        paused = true;
        // Reduce refresh rate to save power — full 90 Hz is only needed while actively tracking.
        trySetPeakRefreshRate(72.0f, "onPause");
        // Note: native render loop and wake lock remain alive here.
        // Pimax XR entry pauses this activity while keeping the render thread running.
        Log.i(TAG, "keeping native render loop and wake lock alive after onPause; Pimax XR entry pauses the activity");
        super.onPause();
    }

    /**
     * Called when the activity is no longer visible.
     *
     * <p>Same semantics as {@link #onPause} — the wake lock and native render loop are kept
     * alive because the Pimax XR entry can stop this activity while the render thread
     * continues in the background.
     */
    @Override
    protected void onStop() {
        Log.i(TAG, "VrRenderActivity.onStop");
        paused = true;
        // Note: native render loop and wake lock remain alive here.
        Log.i(TAG, "keeping native render loop and wake lock alive after onStop; Pimax XR entry can stop the activity");
        super.onStop();
    }

    /**
     * Called when the activity is being destroyed.
     *
     * <p>This is the main cleanup point:
     * <ul>
     *   <li>Requests native shutdown so the render thread terminates gracefully.</li>
     *   <li>Unregisters the proximity sensor, Pimax hardware bridge, and screen receiver
     *       to prevent callbacks after the activity is dead.</li>
     *   <li>Releases the wake lock so the device can sleep.</li>
     * </ul>
     */
    @Override
    protected void onDestroy() {
        Log.i(TAG, "VrRenderActivity.onDestroy");
        paused = true;
        // Signal the native render loop to shut down.
        requestNativeShutdown("onDestroy");
        stopControllerPoller();
        stopDisplayWakeRetry("onDestroy");
        unregisterProximitySensor();
        unregisterPimaxHardwareBridge();
        unregisterScreenReceiver();
        releaseScreenWakeLock();
        super.onDestroy();
    }

    // =========================================================================================
    // Window focus and key handling
    // =========================================================================================

    /**
     * Called when the activity's window gains or loses focus.
     *
     * <p>When focus is gained, applies immersive mode (hides system bars) and keeps the
     * wake lock active so the display stays on during the VR session.
     *
     * @param hasFocus true if the window now has focus, false if it has lost focus
     */
    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            Log.i(TAG, "VrRenderActivity.onWindowFocusChanged(true)");
            // Apply immersive sticky mode — hides navigation and status bars.
            getWindow().getDecorView().setSystemUiVisibility(SYSTEM_UI_VISIBILITY_FLAGS);
            getWindow().addFlags(WINDOW_FLAGS_ON_FOCUS);
            acquireScreenWakeLock("window focus");
        }
    }

    /**
     * Intercepts key events to detect VR controller or hardware buttons that should trigger
     * shutdown of the ALVR session.
     *
     * <p>Exit keys: Back, Escape, Menu, Select (OK), Start (menu).
     * These keys are safe to intercept because the VR session is self-contained and
     * does not rely on Android navigation keys during normal operation.
     *
     * @param event the key event to dispatch
     * @return true if the key was an exit key and the shutdown was triggered; otherwise
     *         delegates to the super implementation
     */
    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        logAnyControllerKeyEvent(event);
        if (event != null && controllerStateForDevice(event.getDevice()) != null) {
            if (handleControllerKeyEvent(event)) {
                return true;
            }
        }
        if (event != null && event.getAction() == KeyEvent.ACTION_UP && isExitKey(event.getKeyCode())) {
            String keyName = KeyEvent.keyCodeToString(event.getKeyCode());
            Log.i(TAG, "handling exit key: " + keyName);
            shutdownAndFinish("key " + keyName);
            return true;
        }
        return super.dispatchKeyEvent(event);
    }

    @Override
    public boolean dispatchGenericMotionEvent(MotionEvent event) {
        logAnyControllerMotionEvent(event);
        if (event != null && handleControllerMotionEvent(event)) {
            return true;
        }
        return super.dispatchGenericMotionEvent(event);
    }

    /**
     * Called when the user presses the system Back button.
     *
     * <p>Triggers a clean shutdown of the ALVR session rather than navigating backwards,
     * since there is no Android navigation hierarchy in a VR session.
     */
    @Override
    public void onBackPressed() {
        Log.i(TAG, "VrRenderActivity.onBackPressed");
        shutdownAndFinish("onBackPressed");
    }

    /**
     * Determines whether a key code corresponds to one of the known VR exit keys.
     *
     * @param keyCode the Android key code (e.g., {@link KeyEvent#KEYCODE_BACK})
     * @return true if pressing this key should exit the VR session
     */
    private boolean isExitKey(int keyCode) {
        switch (keyCode) {
            case KeyEvent.KEYCODE_BACK:
            case KeyEvent.KEYCODE_ESCAPE:
            case KeyEvent.KEYCODE_MENU:
            case KeyEvent.KEYCODE_BUTTON_SELECT:
            case KeyEvent.KEYCODE_BUTTON_START:
                return true;
            default:
                return false;
        }
    }

    // =========================================================================================
    // Shutdown helpers
    // =========================================================================================

    /**
     * Transitions the activity to a shutdown state: marks the activity as paused,
     * requests native shutdown, unregisters receivers, releases the wake lock, and
     * finishes the activity.
     *
     * @param reason a human-readable reason string used in logging (e.g., "key BACK")
     */
    private void shutdownAndFinish(String reason) {
        paused = true;
        setPimaxEyechipEnabled(true, "explicit shutdown: " + reason);
        requestNativeShutdown(reason);
        unregisterScreenReceiver();
        releaseScreenWakeLock();
        finishActivity(reason);
    }

    /**
     * Actually finishes the activity, choosing the appropriate finish method based on
     * Android version. On Lollipop and above, uses {@link #finishAndRemoveTask} to
     * fully remove the activity from the recents list; on older versions uses plain
     * {@link #finish}.
     *
     * @param reason a human-readable reason string used in logging
     */
    private void finishActivity(String reason) {
        if (isFinishing()) {
            // Avoid double-finish if called multiple times.
            return;
        }
        Log.i(TAG, "finishing activity: " + reason);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            // Lollipop+ allows removing the task from recents, giving a cleaner UX.
            finishAndRemoveTask();
        } else {
            finish();
        }
    }

    // =========================================================================================
    // Display refresh rate management
    // =========================================================================================

    /**
     * Attempts to set the display's peak refresh rate.
     *
     * <p>This is a best-effort operation — on some devices or when the app lacks the
     * {@code WRITE_SETTINGS} permission, the call may fail silently. Failures are logged
     * at warning level but do not affect the activity lifecycle.
     *
     * <p>Typical values: 90 Hz during active VR use for smooth head tracking, 72 Hz
     * during pauses to conserve battery.
     *
     * @param refreshRate the desired peak refresh rate in Hz (e.g., 90.0f or 72.0f)
     * @param reason      a human-readable reason for this change (logged alongside success/failure)
     */
    private void trySetPeakRefreshRate(float refreshRate, String reason) {
        String currentValue = Settings.System.getString(getContentResolver(), "peak_refresh_rate");
        if (currentValue != null) {
            try {
                if (Math.abs(Float.parseFloat(currentValue) - refreshRate) < 0.01f) {
                    Log.i(TAG, "peak_refresh_rate already " + currentValue + ": " + reason);
                    return;
                }
            } catch (NumberFormatException ignored) {
                // Fall through and try writing the requested value.
            }
        }
        if (peakRefreshRateWriteDenied) {
            Log.i(TAG, "skipping peak_refresh_rate=" + refreshRate
                    + " after previous denial; current=" + currentValue + ": " + reason);
            return;
        }
        try {
            Settings.System.putFloat(getContentResolver(), "peak_refresh_rate", refreshRate);
            Log.i(TAG, "requested peak_refresh_rate=" + refreshRate + ": " + reason);
        } catch (RuntimeException error) {
            peakRefreshRateWriteDenied = true;
            Log.w(TAG, "failed to set peak_refresh_rate=" + refreshRate + ": " + reason
                    + " (" + error.getClass().getSimpleName() + ": " + error.getMessage() + ")");
        }
    }

    // =========================================================================================
    // Wake lock management
    // =========================================================================================

    /**
     * Creates (if not already created) a {@link PowerManager.WakeLock} that keeps the
     * CPU and display on throughout the VR session.
     *
     * <p>The wake lock is created with the following flags:
     * <ul>
     *   <li>{@code FULL_WAKE_LOCK} — keeps the screen on at full brightness and the CPU active.</li>
     *   <li>{@code ACQUIRE_CAUSES_WAKEUP} — ensures the display turns on immediately when
     *       the wake lock is acquired, even if it was off.</li>
     *   <li>{@code ON_AFTER_RELEASE} — keeps the display on briefly after the wake lock is
     *       released, providing a smoother user experience.</li>
     * </ul>
     *
     * <p>The wake lock is reference-counted disabled (via {@code setReferenceCounted(false)})
     * so that multiple calls to {@link #acquireScreenWakeLock} do not require matching releases.
     */
    @SuppressWarnings("deprecation")
    private void createScreenWakeLock() {
        if (screenWakeLock != null) {
            return;
        }
        PowerManager powerManager = (PowerManager) getSystemService(POWER_SERVICE);
        if (powerManager == null) {
            Log.w(TAG, "PowerManager unavailable; cannot create activity wake lock");
            return;
        }
        int flags = PowerManager.FULL_WAKE_LOCK
                | PowerManager.ACQUIRE_CAUSES_WAKEUP
                | PowerManager.ON_AFTER_RELEASE;
        screenWakeLock = powerManager.newWakeLock(flags, "PimaxALVR:ActivityWakeLock");
        // Disable reference counting so multiple acquires / releases are independent.
        // This simplifies lifecycle management at the cost of requiring explicit release.
        screenWakeLock.setReferenceCounted(false);
    }

    /**
     * Acquires the screen wake lock, creating it first if necessary.
     *
     * <p>Acquiring the wake lock prevents the system from suspending the device or turning
     * off the display while the VR session is active. A timeout ({@link #WAKE_LOCK_TIMEOUT_MS})
     * is set as a safety net to prevent battery drain if the activity becomes orphaned.
     *
     * @param reason a human-readable reason for this acquisition, logged for debugging
     */
    private void acquireScreenWakeLock(String reason) {
        if (screenWakeLock == null) {
            createScreenWakeLock();
        }
        if (screenWakeLock == null) {
            return;
        }
        try {
            screenWakeLock.acquire(WAKE_LOCK_TIMEOUT_MS);
            Log.i(TAG, "acquired activity wake lock: " + reason);
        } catch (RuntimeException error) {
            Log.w(TAG, "failed to acquire activity wake lock: " + reason, error);
        }
    }

    /**
     * Toggles the same vendor display/proximity policy switch used by stock WiFi Airlink.
     *
     * <p>Airlink writes {@code eyechip_on} from {@code HmdSync.registerHw(...)} before it
     * starts streaming. On Crystal OG builds this appears to control the headset's
     * eye/proximity chip policy that can blank the panel while the app keeps streaming.
     * During active ALVR sessions we keep it disabled and restore it only on explicit exit.
     */
    private void setPimaxEyechipEnabled(boolean enabled, String reason) {
        int value = enabled ? 1 : 0;
        String currentValue = Settings.System.getString(getContentResolver(), PIMAX_EYECHIP_ON_SETTING);
        if (Integer.toString(value).equals(currentValue)) {
            Log.i(TAG, PIMAX_EYECHIP_ON_SETTING + " already " + value + ": " + reason);
            return;
        }
        if (pimaxEyechipWriteDenied) {
            Log.i(TAG, "skipping " + PIMAX_EYECHIP_ON_SETTING + "=" + value
                    + " after previous denial; current=" + currentValue + ": " + reason);
            return;
        }
        try {
            boolean ok = Settings.System.putInt(getContentResolver(), PIMAX_EYECHIP_ON_SETTING, value);
            Log.i(TAG, "requested " + PIMAX_EYECHIP_ON_SETTING + "=" + value
                    + " ok=" + ok + ": " + reason);
        } catch (RuntimeException error) {
            pimaxEyechipWriteDenied = true;
            Log.w(TAG, "failed to set " + PIMAX_EYECHIP_ON_SETTING + "=" + value
                    + ": " + reason + " (" + error.getClass().getSimpleName()
                    + ": " + error.getMessage() + ")");
        }
    }

    /**
     * Forces a fresh wake event by releasing and recreating the wake lock.
     *
     * <p>Calling {@link #acquireScreenWakeLock} on an already-held wake lock is not always
     * sufficient to wake the panel back up after the headset has blanked it. Releasing the
     * current lock first ensures the subsequent acquire re-triggers {@code ACQUIRE_CAUSES_WAKEUP}.
     */
    private void forceScreenWakeLock(String reason) {
        if (screenWakeLock != null) {
            try {
                if (screenWakeLock.isHeld()) {
                    screenWakeLock.release();
                    Log.i(TAG, "released activity wake lock before forced wake: " + reason);
                }
            } catch (RuntimeException error) {
                Log.w(TAG, "failed to release activity wake lock before forced wake: " + reason, error);
            } finally {
                screenWakeLock = null;
            }
        }
        acquireScreenWakeLock(reason);
    }

    /**
     * Pokes the system PowerManager to wake the display back up.
     *
     * <p>Pimax appears to treat the proximity transition as a stronger display policy event
     * than a standard Android screen-off, so a wake lock alone is not always sufficient.
     * We try the available hidden PowerManager wake methods reflectively and fall back to
     * the normal wake-lock path if none are available.
     */
    private void pokeDisplayPower(String reason) {
        PowerManager powerManager = (PowerManager) getSystemService(POWER_SERVICE);
        if (powerManager == null) {
            Log.w(TAG, "PowerManager unavailable; cannot poke display power: " + reason);
            return;
        }
        long now = SystemClock.uptimeMillis();
        String packageName = getPackageName();
        if (tryInvokePowerManagerMethod(powerManager, "wakeUp", now, reason, packageName)) {
            return;
        }
        Log.w(TAG, "PowerManager wake methods unavailable; relying on wake lock: " + reason);
    }

    /**
     * Tries to invoke a hidden PowerManager wake helper by name.
     */
    private boolean tryInvokePowerManagerMethod(
            PowerManager powerManager, String methodName, long now, String reason, String packageName) {
        if ("wakeUp".equals(methodName) && powerWakeUpDenied) {
            return false;
        }
        for (Method method : powerManager.getClass().getMethods()) {
            if (!methodName.equals(method.getName())) {
                continue;
            }
            try {
                Class<?>[] parameterTypes = method.getParameterTypes();
                Object[] args =
                        buildPowerManagerInvocationArgs(parameterTypes, now, reason, packageName);
                if (args == null) {
                    continue;
                }
                method.setAccessible(true);
                method.invoke(powerManager, args);
                Log.i(TAG, "poked display power via PowerManager." + methodName + ": " + reason);
                return true;
            } catch (ReflectiveOperationException | IllegalArgumentException error) {
                if ("wakeUp".equals(methodName) && hasSecurityExceptionCause(error)) {
                    powerWakeUpDenied = true;
                    Log.i(TAG, "PowerManager.wakeUp denied; relying on wake lock instead: " + reason);
                    return false;
                }
                Log.w(TAG, "failed to invoke PowerManager." + methodName + ": " + reason, error);
            }
        }
        return false;
    }

    private static boolean hasSecurityExceptionCause(Throwable error) {
        Throwable current = error;
        while (current != null) {
            if (current instanceof SecurityException) {
                return true;
            }
            current = current.getCause();
        }
        return false;
    }

    /**
     * Starts a short retry loop that keeps poking the display while the headset remains near.
     */
    private void startDisplayWakeRetry(String reason) {
        if (screenWakeHandler == null) {
            screenWakeHandler = new Handler(Looper.getMainLooper());
        }
        screenWakeRetryCount = 0;
        screenWakeHandler.removeCallbacks(screenWakeRetryRunnable);
        screenWakeHandler.post(screenWakeRetryRunnable);
        Log.i(TAG, "started display wake retry: " + reason);
    }

    /** Stops any pending display wake retries. */
    private void stopDisplayWakeRetry(String reason) {
        if (screenWakeHandler == null) {
            return;
        }
        screenWakeHandler.removeCallbacks(screenWakeRetryRunnable);
        Log.i(TAG, "stopped display wake retry: " + reason);
    }

    /** Retry wake pokes until the display becomes interactive or the retry budget is used up. */
    private void retryDisplayWakeIfNeeded() {
        if (!headsetNear) {
            return;
        }
        PowerManager powerManager = (PowerManager) getSystemService(POWER_SERVICE);
        if (powerManager != null && powerManager.isInteractive()) {
            Log.i(TAG, "display became interactive; wake retry loop complete");
            stopDisplayWakeRetry("display interactive");
            return;
        }
        if (screenWakeRetryCount >= 20) {
            Log.w(TAG, "display wake retry exhausted; leaving wake lock asserted");
            return;
        }
        screenWakeRetryCount++;
        pokeDisplayPower("retry " + screenWakeRetryCount);
        forceScreenWakeLock("retry " + screenWakeRetryCount);
        if (screenWakeHandler != null) {
            screenWakeHandler.postDelayed(screenWakeRetryRunnable, 250L);
        }
    }

    private Object[] buildPowerManagerInvocationArgs(
            Class<?>[] parameterTypes, long now, String reason, String packageName) {
        Object[] args = new Object[parameterTypes.length];
        for (int index = 0; index < parameterTypes.length; index++) {
            Class<?> parameterType = parameterTypes[index];
            if (parameterType == long.class || parameterType == Long.class) {
                args[index] = now;
            } else if (parameterType == int.class || parameterType == Integer.class) {
                args[index] = 0;
            } else if (parameterType == boolean.class || parameterType == Boolean.class) {
                args[index] = Boolean.TRUE;
            } else if (parameterType == String.class) {
                args[index] = index == parameterTypes.length - 1 ? packageName : reason;
            } else {
                return null;
            }
        }
        return args;
    }

    /**
     * Releases the screen wake lock and clears the reference.
     *
     * <p>Checks whether the wake lock is currently held before releasing to avoid
     * unnecessary warnings. After release, clears the field so a new wake lock will
     * be created on the next acquire if needed.
     */
    private void releaseScreenWakeLock() {
        if (screenWakeLock == null) {
            Log.i(TAG, "activity wake lock already released");
            return;
        }
        try {
            if (screenWakeLock.isHeld()) {
                screenWakeLock.release();
                Log.i(TAG, "released activity wake lock");
            } else {
                Log.i(TAG, "activity wake lock was not held");
            }
        } catch (RuntimeException error) {
            Log.w(TAG, "failed to release activity wake lock", error);
        }
        screenWakeLock = null;
    }

    // =========================================================================================
    // Pimax hardware bridge (IPD sync via Java reflection)
    // =========================================================================================

    /**
     * Registers a listener with the Pimax hardware manager to receive IPD and lens-change events.
     *
     * <p>This method uses Java reflection to interact with the Pimax framework's
     * {@code IPmxHwEventListener} interface and hardware manager service, avoiding a
     * compile-time dependency on the Pimax SDK. The registration is idempotent — calling
     * multiple times has no effect after the first successful registration.
     *
     * <p>Events received:
     * <ul>
     *   <li>{@link #PMX_EVENT_TYPE_MOTOR} — triggered when the user turns the IPD adjustment
     *       knob on the headset. The payload is a string containing the new IPD value in mm
     *       (parsed via {@link Float#parseFloat}). This is forwarded to the native layer via
     *       {@link #nativeNotifyIpdChange}.</li>
     *   <li>{@link #PMX_EVENT_TYPE_LENS_CHANGED} — triggered when a different prescription
     *       lens module is inserted. Used for logging; no native callback is made.</li>
     * </ul>
     *
     * <p>The listener is implemented as a dynamic {@link Proxy} whose {@code asBinder()}
     * returns a custom {@link Binder} that handles the actual IPC. This is necessary because
     * the system wants a concrete {@link Binder} for the AIDL callback, not a proxy object.
     */
    private void registerPimaxHardwareBridge() {
        if (pmxHwRegistered) {
            return;
        }
        // "pmx_hw" is the Pimax hardware manager system service name.
        Object hwManager = getSystemService("pmx_hw");
        if (hwManager == null) {
            Log.w(TAG, "Pimax hardware manager unavailable; IPD sync disabled");
            return;
        }
        try {
            // Load the IPmxHwEventListener interface class via reflection.
            Class<?> listenerClass = Class.forName(PMX_HW_LISTENER_DESCRIPTOR);

            // Create a Binder that acts as the real listener backend. The proxy's asBinder()
            // will delegate to this binder, allowing the system to hold a reference across IPC.
            pmxHwCallbackBinder = new Binder() {
                /**
                 * Handles incoming transactions from the Pimax hardware manager.
                 *
                 * <p>Transaction code 1 carries the hardware event data: type, value, and an
                 * optional string payload (e.g., IPD string for motor events).
                 *
                 * @param code     the transaction code (1 = hardware event)
                 * @param data     the inbound Parcel data
                 * @param reply    the outbound Parcel reply (unused)
                 * @param flags    transaction flags
                 * @return true if the transaction was handled
                 * @throws RemoteException if the transaction dispatch fails
                 */
                @Override
                protected boolean onTransact(int code, Parcel data, Parcel reply, int flags)
                        throws RemoteException {
                    // Strictly enforce the interface for codes in the valid range.
                    if (code >= 1 && code <= 16777215) {
                        data.enforceInterface(PMX_HW_LISTENER_DESCRIPTOR);
                    }
                    if (code == 1) {
                        // Parse hardware event: type, value, payload string.
                        int type = data.readInt();
                        int value = data.readInt();
                        String payload = data.readString();
                        onPimaxHwEvent(type, value, payload);
                        return true;
                    }
                    return super.onTransact(code, data, reply, flags);
                }
            };

            // Build an InvocationHandler that routes interface method calls to onTransact
            // via the Binder we created above. The three Object methods (asBinder, toString,
            // hashCode/equals) are handled explicitly; all other interface methods return null
            // as they should never be called.
            InvocationHandler handler = (proxy, method, args) -> {
                String name = method.getName();
                if ("asBinder".equals(name)) {
                    // The system calls asBinder() to get a stable Binder reference for this listener.
                    return pmxHwCallbackBinder;
                }
                if ("toString".equals(name)) {
                    return "PimaxHwEventListenerProxy";
                }
                if ("hashCode".equals(name)) {
                    return System.identityHashCode(proxy);
                }
                if ("equals".equals(name)) {
                    // Check if the given object is this same proxy instance.
                    Object other = args != null && args.length > 0 ? args[0] : null;
                    return proxy == other;
                }
                // All other interface methods should not be invoked in normal operation.
                return null;
            };

            // Create the dynamic proxy instance.
            pmxHwListenerProxy =
                    Proxy.newProxyInstance(
                            VrRenderActivity.class.getClassLoader(),
                            new Class<?>[] {listenerClass},
                            handler);

            // Register the listener with the Pimax hardware manager.
            // The third parameter (PMX_EVENT_TYPE_MOTOR_AND_LENS) selects which event types
            // the caller wants to receive.
            Method registerMethod = hwManager.getClass().getMethod("registerListener", listenerClass, int.class);
            Object result = registerMethod.invoke(hwManager, pmxHwListenerProxy, PMX_EVENT_TYPE_MOTOR_AND_LENS);
            if (result instanceof Boolean && !((Boolean) result)) {
                Log.w(TAG, "Pimax hardware listener registration returned false");
                return;
            }

            // Store references so we can unregister later.
            pmxHwManager = hwManager;
            pmxHwListenerClass = listenerClass;
            pmxHwRegistered = true;
            Log.i(TAG, "registered Pimax hardware listener for IPD sync");
        } catch (ReflectiveOperationException error) {
            Log.w(TAG, "failed to register Pimax hardware listener", error);
        }
    }

    /**
     * Unregisters the Pimax hardware listener previously registered in
     * {@link #registerPimaxHardwareBridge}.
     *
     * <p>This is called during {@link #onDestroy} to clean up the hardware bridge before
     * the activity is destroyed. After unregistration, the Pimax manager will no longer
     * dispatch events to this activity.
     */
    private void unregisterPimaxHardwareBridge() {
        if (!pmxHwRegistered || pmxHwManager == null || pmxHwListenerProxy == null || pmxHwListenerClass == null) {
            return;
        }
        try {
            Method unregisterMethod =
                    pmxHwManager.getClass().getMethod("unregisterListener", pmxHwListenerClass, int.class);
            unregisterMethod.invoke(pmxHwManager, pmxHwListenerProxy, PMX_EVENT_TYPE_MOTOR_AND_LENS);
            Log.i(TAG, "unregistered Pimax hardware listener");
        } catch (ReflectiveOperationException error) {
            Log.w(TAG, "failed to unregister Pimax hardware listener", error);
        } finally {
            // Always clear state, even on error.
            pmxHwRegistered = false;
            pmxHwManager = null;
            pmxHwListenerProxy = null;
            pmxHwListenerClass = null;
            pmxHwCallbackBinder = null;
        }
    }

    /**
     * Handles incoming Pimax hardware events dispatched from the
     * {@link Binder.Proxy#onTransact} callback.
     *
     * <p>Two event types are handled:
     * <ul>
     *   <li>{@link #PMX_EVENT_TYPE_MOTOR}: the IPD adjustment knob was turned. The payload
     *       is a string containing the new IPD in mm (e.g., "63.5"). Parsed and forwarded
     *       to the native layer via {@link #nativeNotifyIpdChange}.</li>
     *   <li>{@link #PMX_EVENT_TYPE_LENS_CHANGED}: a different lens module was inserted.
     *       Logged only — no native callback since ALVR does not currently use this information.</li>
     * </ul>
     *
     * @param type    the event type (PMX_EVENT_TYPE_MOTOR or PMX_EVENT_TYPE_LENS_CHANGED)
     * @param value   additional event value (for motor events, 1 = IPD decreased, 2 = IPD increased)
     * @param payload optional string payload (for motor events, the IPD value as a string)
     */
    private void onPimaxHwEvent(int type, int value, String payload) {
        Log.i(TAG, "Pimax hardware event: type=" + type + " value=" + value + " data=" + payload);
        if (type == PMX_EVENT_TYPE_MOTOR && (value == 1 || value == 2) && payload != null) {
            // IPD knob turned — parse the new IPD value and notify native.
            try {
                if (nativeLibrariesLoaded) {
                    nativeNotifyIpdChange(Float.parseFloat(payload));
                }
            } catch (NumberFormatException error) {
                Log.w(TAG, "failed to parse Pimax IPD payload: " + payload, error);
            }
        } else if (type == PMX_EVENT_TYPE_LENS_CHANGED) {
            Log.i(TAG, "Pimax lens change event received: value=" + value);
        }
    }

    // =========================================================================================
    // Proximity sensor
    // =========================================================================================

    /**
     * Registers the proximity sensor listener if not already registered and the sensor
     * is available on the device.
     *
     * <p>The proximity sensor detects when the headset is brought close to the user's
     * face. When near, the native layer may pause video decoding or reduce render quality.
     * When far (removed), full-quality rendering resumes.
     *
     * <p>Registration is idempotent — multiple calls without an intervening unregister
     * have no additional effect.
     */
    private void registerProximitySensor() {
        if (proximityRegistered) {
            return;
        }
        if (sensorManager == null) {
            sensorManager = (SensorManager) getSystemService(SENSOR_SERVICE);
        }
        if (sensorManager == null) {
            Log.w(TAG, "SensorManager unavailable; proximity sync disabled");
            return;
        }
        proximitySensor = sensorManager.getDefaultSensor(Sensor.TYPE_PROXIMITY);
        if (proximitySensor == null) {
            Log.w(TAG, "proximity sensor unavailable; proximity sync disabled");
            return;
        }
        boolean registered = sensorManager.registerListener(
                proximityListener, proximitySensor, SensorManager.SENSOR_DELAY_NORMAL);
        proximityRegistered = registered;
        Log.i(TAG, "registered proximity sensor listener: " + registered);
    }

    /**
     * Unregisters the proximity sensor listener previously registered in
     * {@link #registerProximitySensor}.
     *
     * <p>Called during {@link #onDestroy} to clean up the sensor listener. Also clears
     * the stored references so a fresh registration can occur if the activity is recreated.
     */
    private void unregisterProximitySensor() {
        if (sensorManager == null || !proximityRegistered) {
            return;
        }
        try {
            sensorManager.unregisterListener(proximityListener);
            Log.i(TAG, "unregistered proximity sensor listener");
        } catch (RuntimeException error) {
            Log.w(TAG, "failed to unregister proximity sensor listener", error);
        } finally {
            proximityRegistered = false;
            proximitySensor = null;
        }
    }

    // =========================================================================================
    // Controller polling
    //
    // Pimax controllers ("nrfinput_left" / "nrfinput_right") show up as standard Android
    // InputDevices. Buttons arrive via KeyEvents, analog axes via MotionEvents — both are
    // captured in dispatch overrides above. The ControllerPoller thread:
    //   1. Detects connect/disconnect by enumerating InputDevices.
    //   2. Reads battery from sysfs (paths reverse-engineered from stock Pimax APK).
    //   3. Pushes a state snapshot to native at 30 Hz.
    //
    // Bit-to-button mapping is provisional — diagnostic logs will validate against hardware.
    // =========================================================================================

    private static final long CONTROLLER_POLL_INTERVAL_MS = 33;
    private static final boolean USE_NATIVE_PIMAX_SDK_CONTROLLER_POLLER = true;
    private static final String CONTROLLER_DEVICE_NAME_LEFT = "nrfinput_left";
    private static final String CONTROLLER_DEVICE_NAME_RIGHT = "nrfinput_right";
    private static final String CONTROLLER_BATTERY_PATH_LEFT =
            "/sys/class/pimax_controller/controller_left/battery";
    private static final String CONTROLLER_BATTERY_PATH_RIGHT =
            "/sys/class/pimax_controller/controller_right/battery";

    private static final int HAND_LEFT = 0;
    private static final int HAND_RIGHT = 1;

    // Provisional bit layout — kept in lockstep with controller.rs.
    private static final int BIT_TRIGGER = 1 << 0;
    private static final int BIT_THUMBSTICK_CLICK = 1 << 1;
    private static final int BIT_MENU = 1 << 2;
    private static final int BIT_GRIP = 1 << 3;
    private static final int BIT_AX = 1 << 4;
    private static final int BIT_BY = 1 << 5;

    private static final class ControllerState {
        volatile boolean connected = false;
        volatile int handle = 0;
        volatile int buttonsPressed = 0;
        volatile float trigger = 0f;
        volatile float grip = 0f;
        volatile float thumbstickX = 0f;
        volatile float thumbstickY = 0f;
        volatile int battery = 0;
    }

    private final ControllerState leftController = new ControllerState();
    private final ControllerState rightController = new ControllerState();
    private volatile Thread controllerPoller = null;
    private volatile boolean controllerPollerRunning = false;
    private long controllerLogCounter = 0L;

    private ControllerState controllerStateForDevice(InputDevice device) {
        if (device == null) {
            return null;
        }
        String name = device.getName();
        if (CONTROLLER_DEVICE_NAME_LEFT.equals(name)) {
            return leftController;
        }
        if (CONTROLLER_DEVICE_NAME_RIGHT.equals(name)) {
            return rightController;
        }
        return null;
    }

    private static int mapKeyCodeToBit(int keyCode) {
        switch (keyCode) {
            case KeyEvent.KEYCODE_BUTTON_R1:
            case KeyEvent.KEYCODE_BUTTON_L1:
                return BIT_TRIGGER;
            case KeyEvent.KEYCODE_BUTTON_THUMBL:
            case KeyEvent.KEYCODE_BUTTON_THUMBR:
                return BIT_THUMBSTICK_CLICK;
            case KeyEvent.KEYCODE_MENU:
            case KeyEvent.KEYCODE_BUTTON_START:
            case KeyEvent.KEYCODE_BUTTON_SELECT:
                return BIT_MENU;
            case KeyEvent.KEYCODE_BUTTON_R2:
            case KeyEvent.KEYCODE_BUTTON_L2:
                return BIT_GRIP;
            case KeyEvent.KEYCODE_BUTTON_A:
            case KeyEvent.KEYCODE_BUTTON_X:
                return BIT_AX;
            case KeyEvent.KEYCODE_BUTTON_B:
            case KeyEvent.KEYCODE_BUTTON_Y:
                return BIT_BY;
            default:
                return 0;
        }
    }

    private static String keyActionToString(int action) {
        if (action == KeyEvent.ACTION_DOWN) {
            return "down";
        }
        if (action == KeyEvent.ACTION_UP) {
            return "up";
        }
        return Integer.toString(action);
    }

    private static String motionActionToString(int action) {
        if (action == MotionEvent.ACTION_DOWN) {
            return "down";
        }
        if (action == MotionEvent.ACTION_UP) {
            return "up";
        }
        if (action == MotionEvent.ACTION_MOVE) {
            return "move";
        }
        if (action == MotionEvent.ACTION_CANCEL) {
            return "cancel";
        }
        return Integer.toString(action);
    }

    private static String eventDeviceName(InputDevice device) {
        return device != null ? device.getName() : "<unknown>";
    }

    private static int eventDeviceId(InputDevice device) {
        return device != null ? device.getId() : -1;
    }

    private static String eventSourceString(int source) {
        return "0x" + Integer.toHexString(source);
    }

    private boolean handleControllerKeyEvent(KeyEvent event) {
        ControllerState state = controllerStateForDevice(event.getDevice());
        if (state == null) {
            return false;
        }
        InputDevice device = event.getDevice();
        String deviceName = eventDeviceName(device);
        int deviceId = eventDeviceId(device);
        String source = eventSourceString(event.getSource());
        int before = state.buttonsPressed;
        int bit = mapKeyCodeToBit(event.getKeyCode());
        if (bit == 0) {
            // Unknown key on a known controller device — log once for discovery.
            Log.i(TAG, "unmapped controller key: device=" + deviceName
                    + " deviceId=" + deviceId
                    + " keyCode=" + KeyEvent.keyCodeToString(event.getKeyCode())
                    + " scanCode=" + event.getScanCode()
                    + " action=" + keyActionToString(event.getAction())
                    + " repeat=" + event.getRepeatCount()
                    + " source=" + source);
            return false;
        }
        if (event.getAction() == KeyEvent.ACTION_DOWN) {
            state.buttonsPressed |= bit;
        } else if (event.getAction() == KeyEvent.ACTION_UP) {
            state.buttonsPressed &= ~bit;
        }
        Log.i(TAG, "controller key event: device=" + deviceName
                + " deviceId=" + deviceId
                + " keyCode=" + KeyEvent.keyCodeToString(event.getKeyCode())
                + " scanCode=" + event.getScanCode()
                + " action=" + keyActionToString(event.getAction())
                + " repeat=" + event.getRepeatCount()
                + " source=" + source
                + " mappedBit=0x" + Integer.toHexString(bit)
                + " buttonsBefore=0x" + Integer.toHexString(before)
                + " buttonsAfter=0x" + Integer.toHexString(state.buttonsPressed));
        return true;
    }

    private boolean handleControllerMotionEvent(MotionEvent event) {
        ControllerState state = controllerStateForDevice(event.getDevice());
        if (state == null) {
            return false;
        }
        InputDevice device = event.getDevice();
        String deviceName = eventDeviceName(device);
        int deviceId = eventDeviceId(device);
        String source = eventSourceString(event.getSource());
        float beforeX = state.thumbstickX;
        float beforeY = state.thumbstickY;
        float beforeTrigger = state.trigger;
        // Standard joystick mapping. May need per-device tuning once hardware is observed.
        state.thumbstickX = event.getAxisValue(MotionEvent.AXIS_X);
        state.thumbstickY = event.getAxisValue(MotionEvent.AXIS_Y);
        float trig = event.getAxisValue(MotionEvent.AXIS_LTRIGGER);
        if (trig <= 0f) trig = event.getAxisValue(MotionEvent.AXIS_RTRIGGER);
        if (trig <= 0f) trig = event.getAxisValue(MotionEvent.AXIS_BRAKE);
        if (trig <= 0f) trig = event.getAxisValue(MotionEvent.AXIS_GAS);
        state.trigger = trig;
        Log.i(TAG, "controller motion event: device=" + deviceName
                + " deviceId=" + deviceId
                + " action=" + motionActionToString(event.getAction())
                + " source=" + source
                + " stick=(" + beforeX + "," + beforeY + ")=>("
                + state.thumbstickX + "," + state.thumbstickY + ")"
                + " trigger=" + beforeTrigger + "=>" + state.trigger);
        return true;
    }

    private void logAnyControllerKeyEvent(KeyEvent event) {
        if (event == null) {
            return;
        }
        InputDevice device = event.getDevice();
        Log.i(TAG, "key event seen: device=" + eventDeviceName(device)
                + " deviceId=" + eventDeviceId(device)
                + " keyCode=" + KeyEvent.keyCodeToString(event.getKeyCode())
                + " scanCode=" + event.getScanCode()
                + " action=" + keyActionToString(event.getAction())
                + " repeat=" + event.getRepeatCount()
                + " source=" + eventSourceString(event.getSource()));
    }

    private void logAnyControllerMotionEvent(MotionEvent event) {
        if (event == null) {
            return;
        }
        InputDevice device = event.getDevice();
        Log.i(TAG, "motion event seen: device=" + eventDeviceName(device)
                + " deviceId=" + eventDeviceId(device)
                + " action=" + motionActionToString(event.getAction())
                + " source=" + eventSourceString(event.getSource())
                + " axisX=" + event.getAxisValue(MotionEvent.AXIS_X)
                + " axisY=" + event.getAxisValue(MotionEvent.AXIS_Y)
                + " ltrigger=" + event.getAxisValue(MotionEvent.AXIS_LTRIGGER)
                + " rtrigger=" + event.getAxisValue(MotionEvent.AXIS_RTRIGGER)
                + " brake=" + event.getAxisValue(MotionEvent.AXIS_BRAKE)
                + " gas=" + event.getAxisValue(MotionEvent.AXIS_GAS));
    }

    private static int readControllerBattery(boolean left) {
        File file = new File(left ? CONTROLLER_BATTERY_PATH_LEFT : CONTROLLER_BATTERY_PATH_RIGHT);
        if (!file.exists()) {
            return 0;
        }
        try (BufferedReader reader = new BufferedReader(new FileReader(file))) {
            String line = reader.readLine();
            if (line == null) {
                return 0;
            }
            return Integer.parseInt(line.trim());
        } catch (IOException | NumberFormatException ignored) {
            return 0;
        }
    }

    private void startControllerPoller() {
        if (USE_NATIVE_PIMAX_SDK_CONTROLLER_POLLER) {
            Log.i(TAG, "skipping Android InputDevice ControllerPoller; native Pimax SDK poller owns controller state");
            return;
        }
        if (controllerPoller != null) {
            return;
        }
        controllerPollerRunning = true;
        controllerPoller = new Thread(this::runControllerPoller, "ControllerPoller");
        controllerPoller.setDaemon(true);
        controllerPoller.start();
        Log.i(TAG, "started ControllerPoller @ " + CONTROLLER_POLL_INTERVAL_MS + " ms");
    }

    private void stopControllerPoller() {
        controllerPollerRunning = false;
        Thread t = controllerPoller;
        controllerPoller = null;
        if (t != null) {
            t.interrupt();
            try {
                t.join(200);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            Log.i(TAG, "stopped ControllerPoller");
        }
    }

    private void runControllerPoller() {
        while (controllerPollerRunning) {
            try {
                pollControllersOnce();
            } catch (Throwable t) {
                Log.w(TAG, "ControllerPoller iteration failed", t);
            }
            try {
                Thread.sleep(CONTROLLER_POLL_INTERVAL_MS);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
                break;
            }
        }
    }

    private void pollControllersOnce() {
        boolean leftSeen = false;
        boolean rightSeen = false;
        int[] deviceIds = InputDevice.getDeviceIds();
        for (int id : deviceIds) {
            InputDevice device = InputDevice.getDevice(id);
            if (device == null) {
                continue;
            }
            String name = device.getName();
            if (CONTROLLER_DEVICE_NAME_LEFT.equals(name)) {
                leftSeen = true;
            } else if (CONTROLLER_DEVICE_NAME_RIGHT.equals(name)) {
                rightSeen = true;
            }
        }
        updateConnection(leftController, HAND_LEFT, leftSeen);
        updateConnection(rightController, HAND_RIGHT, rightSeen);

        controllerLogCounter++;
        boolean shouldLog = controllerLogCounter <= 5 || controllerLogCounter % 300 == 0;

        if (leftController.connected) {
            leftController.battery = readControllerBattery(true);
            pushState(HAND_LEFT, leftController, shouldLog);
        }
        if (rightController.connected) {
            rightController.battery = readControllerBattery(false);
            pushState(HAND_RIGHT, rightController, shouldLog);
        }
    }

    private void updateConnection(ControllerState state, int hand, boolean nowConnected) {
        if (nowConnected == state.connected) {
            return;
        }
        state.connected = nowConnected;
        if (!nowConnected) {
            state.buttonsPressed = 0;
            state.trigger = 0f;
            state.grip = 0f;
            state.thumbstickX = 0f;
            state.thumbstickY = 0f;
        }
        Log.i(TAG, "controller " + (hand == HAND_LEFT ? "left" : "right")
                + " connected=" + nowConnected);
        try {
            nativeNotifyControllerConnection(hand, nowConnected);
        } catch (UnsatisfiedLinkError error) {
            Log.w(TAG, "nativeNotifyControllerConnection unavailable", error);
        }
    }

    private void pushState(int hand, ControllerState state, boolean shouldLog) {
        if (shouldLog) {
            Log.i(TAG, "controller state hand=" + hand
                    + " buttons=0x" + Integer.toHexString(state.buttonsPressed)
                    + " trigger=" + state.trigger
                    + " stick=(" + state.thumbstickX + "," + state.thumbstickY + ")"
                    + " battery=" + state.battery);
        }
        try {
            nativeNotifyControllerState(hand, state.handle,
                    state.buttonsPressed, 0,
                    state.trigger, state.grip,
                    state.thumbstickX, state.thumbstickY,
                    state.battery);
        } catch (UnsatisfiedLinkError error) {
            Log.w(TAG, "nativeNotifyControllerState unavailable", error);
        }
    }

    // =========================================================================================
    // Screen broadcast receiver registration
    // =========================================================================================

    /**
     * Registers the {@link #screenReceiver} for screen-on, screen-off, and shutdown broadcasts.
     *
     * <p>Registration is idempotent to handle cases where multiple entry points (onCreate,
     * onResume) could trigger registration. The {@code screenReceiverRegistered} flag
     * tracks the state.
     */
    private void registerScreenReceiver() {
        if (screenReceiverRegistered) {
            return;
        }
        IntentFilter filter = new IntentFilter();
        filter.addAction(Intent.ACTION_SCREEN_OFF);
        filter.addAction(Intent.ACTION_SCREEN_ON);
        filter.addAction(ACTION_SHUTDOWN);
        registerReceiver(screenReceiver, filter);
        screenReceiverRegistered = true;
        Log.i(TAG, "registered screen receiver");
    }

    /**
     * Unregisters the screen broadcast receiver.
     *
     * <p>Called during shutdown and activity destroy. Uses a try/catch because
     * unregistering an already-unregistered receiver throws a
     * {@link RuntimeException}.
     */
    private void unregisterScreenReceiver() {
        if (!screenReceiverRegistered) {
            return;
        }
        try {
            unregisterReceiver(screenReceiver);
        } catch (RuntimeException error) {
            Log.w(TAG, "failed to unregister screen receiver", error);
        }
        screenReceiverRegistered = false;
    }

    // =========================================================================================
    // Native shutdown coordination
    // =========================================================================================

    /**
     * Records the shutdown request and calls the native shutdown hook.
     *
     * <p>This method is called at multiple points in the activity lifecycle (e.g., exit key
     * pressed, shutdown broadcast received, onDestroy). The native shutdown is idempotent —
     * calling it multiple times has no additional effect on the native side.
     *
     * <p>Any {@link UnsatisfiedLinkError} or {@link RuntimeException} from the JNI call is
     * caught and logged — the activity will still finish gracefully even if the native
     * hook is unavailable (e.g., during early startup before the native library is fully loaded).
     *
     * @param reason a human-readable reason for the shutdown, included in log messages
     */
    private void requestNativeShutdown(String reason) {
        nativeShutdownRequested = true;
        if (!nativeLibrariesLoaded) {
            Log.i(TAG, "deferring native shutdown until libraries load: " + reason);
            return;
        }
        try {
            nativeRequestShutdown();
            Log.i(TAG, "requested native shutdown: " + reason);
        } catch (UnsatisfiedLinkError error) {
            Log.w(TAG, "native shutdown hook unavailable: " + reason, error);
        } catch (RuntimeException error) {
            Log.w(TAG, "native shutdown hook failed: " + reason, error);
        }
    }

    /**
     * Clears the native shutdown flag so the render loop can run again after a previous
     * shutdown request.
     *
     * <p>Called in {@link #onCreate} and {@link #onResume} to ensure the native layer is
     * ready to render. If the native hook is unavailable (e.g., library not yet loaded),
     * the error is logged but does not affect the activity lifecycle.
     *
     * @param reason a human-readable reason for the reset, included in log messages
     */
    private void resetNativeShutdown(String reason) {
        if (!nativeLibrariesLoaded) {
            Log.i(TAG, "deferring native reset until libraries load: " + reason);
            return;
        }
        try {
            nativeResetShutdown();
            Log.i(TAG, "reset native shutdown: " + reason);
        } catch (UnsatisfiedLinkError error) {
            Log.w(TAG, "native reset hook unavailable: " + reason, error);
        } catch (RuntimeException error) {
            Log.w(TAG, "native reset hook failed: " + reason, error);
        }
    }
}
