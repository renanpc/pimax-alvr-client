package com.pimax.alvr.client;

import android.app.NativeActivity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Build;
import android.os.Binder;
import android.os.Bundle;
import android.os.Parcel;
import android.os.PowerManager;
import android.os.RemoteException;
import android.provider.Settings;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.util.Log;
import android.view.KeyEvent;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

public final class VrRenderActivity extends NativeActivity {
    private static final String TAG = "PimaxALVRActivity";
    private static final int WINDOW_FLAGS_ON_CREATE = 132;
    private static final int WINDOW_FLAGS_ON_FOCUS = 128;
    private static final int SYSTEM_UI_VISIBILITY_FLAGS = 5894;
    private static final long WAKE_LOCK_TIMEOUT_MS = 10 * 60 * 1000L;
    private static final String ACTION_SHUTDOWN = "com.pimax.alvr.client.ACTION_SHUTDOWN";
    private static final String PMX_HW_LISTENER_DESCRIPTOR = "android.app.pmx.IPmxHwEventListener";
    private static final int PMX_EVENT_TYPE_MOTOR = 1;
    private static final int PMX_EVENT_TYPE_LENS_CHANGED = 2;
    private static final int PMX_EVENT_TYPE_MOTOR_AND_LENS = 3;

    private PowerManager.WakeLock screenWakeLock;
    private boolean screenReceiverRegistered;
    private boolean paused;
    private boolean nativeShutdownRequested;
    private Object pmxHwManager;
    private Object pmxHwListenerProxy;
    private Class<?> pmxHwListenerClass;
    private Binder pmxHwCallbackBinder;
    private boolean pmxHwRegistered;
    private SensorManager sensorManager;
    private Sensor proximitySensor;
    private boolean proximityRegistered;
    private final SensorEventListener proximityListener = new SensorEventListener() {
        @Override
        public void onSensorChanged(SensorEvent event) {
            float distance = event != null && event.values.length > 0 ? event.values[0] : Float.NaN;
            nativeNotifyProximity(distance < 1.0f);
        }

        @Override
        public void onAccuracyChanged(Sensor sensor, int accuracy) {
        }
    };

    static {
        try {
            System.loadLibrary("pxrapi");
            Log.i(TAG, "loaded pxrapi");
        } catch (UnsatisfiedLinkError error) {
            Log.w(TAG, "pxrapi library is not in this APK path; continuing with framework-loaded PxrApi", error);
        }
        System.loadLibrary("pimax_alvr_client");
    }

    private final BroadcastReceiver screenReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent != null ? intent.getAction() : null;
            Log.i(TAG, "screenReceiver.onReceive(" + action + ")");
            if (Intent.ACTION_SCREEN_ON.equals(action)) {
                nativeNotifyScreen(true);
                acquireScreenWakeLock("screen-on broadcast");
            } else if (Intent.ACTION_SCREEN_OFF.equals(action)) {
                nativeNotifyScreen(false);
                Log.i(TAG, "screen turned off; keeping app running for development");
                // Note: stock AirLink shuts down here, but we keep running for testing
            } else if (ACTION_SHUTDOWN.equals(action)) {
                Log.i(TAG, "received ALVR shutdown broadcast");
                shutdownAndFinish("shutdown broadcast");
            }
        }
    };

    private static native void nativeRequestShutdown();
    private static native void nativeResetShutdown();
    private static native void nativeNotifyIpdChange(float rawIpd);
    private static native void nativeNotifyProximity(boolean isNear);
    private static native void nativeNotifyScreen(boolean isScreenOn);

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "VrRenderActivity.onCreate");
        paused = false;
        nativeShutdownRequested = false;
        resetNativeShutdown("onCreate");
        getWindow().addFlags(WINDOW_FLAGS_ON_CREATE);
        createScreenWakeLock();
        registerScreenReceiver();
        acquireScreenWakeLock("onCreate");
        nativeNotifyScreen(true);
        registerPimaxHardwareBridge();
        registerProximitySensor();
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "VrRenderActivity.onResume");
        paused = false;
        if (nativeShutdownRequested) {
            Log.i(TAG, "native shutdown already requested; finishing instead of resuming");
            finishActivity("resume after native shutdown");
            return;
        }
        resetNativeShutdown("onResume");
        trySetPeakRefreshRate(90.0f, "onResume");
        getWindow().addFlags(WINDOW_FLAGS_ON_CREATE | WINDOW_FLAGS_ON_FOCUS);
        registerScreenReceiver();
        acquireScreenWakeLock("onResume");
        nativeNotifyScreen(true);
        registerPimaxHardwareBridge();
        registerProximitySensor();
    }

    @Override
    protected void onPause() {
        Log.i(TAG, "VrRenderActivity.onPause");
        paused = true;
        trySetPeakRefreshRate(72.0f, "onPause");
        Log.i(TAG, "keeping native render loop and wake lock alive after onPause; Pimax XR entry pauses the activity");
        super.onPause();
    }

    @Override
    protected void onStop() {
        Log.i(TAG, "VrRenderActivity.onStop");
        paused = true;
        Log.i(TAG, "keeping native render loop and wake lock alive after onStop; Pimax XR entry can stop the activity");
        super.onStop();
    }

    @Override
    protected void onDestroy() {
        Log.i(TAG, "VrRenderActivity.onDestroy");
        paused = true;
        requestNativeShutdown("onDestroy");
        unregisterProximitySensor();
        unregisterPimaxHardwareBridge();
        unregisterScreenReceiver();
        releaseScreenWakeLock();
        super.onDestroy();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            Log.i(TAG, "VrRenderActivity.onWindowFocusChanged(true)");
            getWindow().getDecorView().setSystemUiVisibility(SYSTEM_UI_VISIBILITY_FLAGS);
            getWindow().addFlags(WINDOW_FLAGS_ON_FOCUS);
            acquireScreenWakeLock("window focus");
        }
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        if (event != null && event.getAction() == KeyEvent.ACTION_UP && isExitKey(event.getKeyCode())) {
            String keyName = KeyEvent.keyCodeToString(event.getKeyCode());
            Log.i(TAG, "handling exit key: " + keyName);
            shutdownAndFinish("key " + keyName);
            return true;
        }
        return super.dispatchKeyEvent(event);
    }

    @Override
    public void onBackPressed() {
        Log.i(TAG, "VrRenderActivity.onBackPressed");
        shutdownAndFinish("onBackPressed");
    }

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

    private void shutdownAndFinish(String reason) {
        paused = true;
        requestNativeShutdown(reason);
        unregisterScreenReceiver();
        releaseScreenWakeLock();
        finishActivity(reason);
    }

    private void finishActivity(String reason) {
        if (isFinishing()) {
            return;
        }
        Log.i(TAG, "finishing activity: " + reason);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            finishAndRemoveTask();
        } else {
            finish();
        }
    }

    private void trySetPeakRefreshRate(float refreshRate, String reason) {
        try {
            Settings.System.putFloat(getContentResolver(), "peak_refresh_rate", refreshRate);
            Log.i(TAG, "requested peak_refresh_rate=" + refreshRate + ": " + reason);
        } catch (RuntimeException error) {
            Log.w(TAG, "failed to set peak_refresh_rate=" + refreshRate + ": " + reason
                    + " (" + error.getClass().getSimpleName() + ": " + error.getMessage() + ")");
        }
    }

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
        screenWakeLock.setReferenceCounted(false);
    }

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

    private void registerPimaxHardwareBridge() {
        if (pmxHwRegistered) {
            return;
        }
        Object hwManager = getSystemService("pmx_hw");
        if (hwManager == null) {
            Log.w(TAG, "Pimax hardware manager unavailable; IPD sync disabled");
            return;
        }
        try {
            Class<?> listenerClass = Class.forName(PMX_HW_LISTENER_DESCRIPTOR);
            pmxHwCallbackBinder = new Binder() {
                @Override
                protected boolean onTransact(int code, Parcel data, Parcel reply, int flags)
                        throws RemoteException {
                    if (code >= 1 && code <= 16777215) {
                        data.enforceInterface(PMX_HW_LISTENER_DESCRIPTOR);
                    }
                    if (code == 1) {
                        int type = data.readInt();
                        int value = data.readInt();
                        String payload = data.readString();
                        onPimaxHwEvent(type, value, payload);
                        return true;
                    }
                    return super.onTransact(code, data, reply, flags);
                }
            };
            InvocationHandler handler = (proxy, method, args) -> {
                String name = method.getName();
                if ("asBinder".equals(name)) {
                    return pmxHwCallbackBinder;
                }
                if ("toString".equals(name)) {
                    return "PimaxHwEventListenerProxy";
                }
                if ("hashCode".equals(name)) {
                    return System.identityHashCode(proxy);
                }
                if ("equals".equals(name)) {
                    Object other = args != null && args.length > 0 ? args[0] : null;
                    return proxy == other;
                }
                return null;
            };
            pmxHwListenerProxy =
                    Proxy.newProxyInstance(
                            VrRenderActivity.class.getClassLoader(),
                            new Class<?>[] {listenerClass},
                            handler);
            Method registerMethod = hwManager.getClass().getMethod("registerListener", listenerClass, int.class);
            Object result = registerMethod.invoke(hwManager, pmxHwListenerProxy, PMX_EVENT_TYPE_MOTOR_AND_LENS);
            if (result instanceof Boolean && !((Boolean) result)) {
                Log.w(TAG, "Pimax hardware listener registration returned false");
                return;
            }
            pmxHwManager = hwManager;
            pmxHwListenerClass = listenerClass;
            pmxHwRegistered = true;
            Log.i(TAG, "registered Pimax hardware listener for IPD sync");
        } catch (ReflectiveOperationException error) {
            Log.w(TAG, "failed to register Pimax hardware listener", error);
        }
    }

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
            pmxHwRegistered = false;
            pmxHwManager = null;
            pmxHwListenerProxy = null;
            pmxHwListenerClass = null;
            pmxHwCallbackBinder = null;
        }
    }

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

    private void onPimaxHwEvent(int type, int value, String payload) {
        Log.i(TAG, "Pimax hardware event: type=" + type + " value=" + value + " data=" + payload);
        if (type == PMX_EVENT_TYPE_MOTOR && (value == 1 || value == 2) && payload != null) {
            try {
                nativeNotifyIpdChange(Float.parseFloat(payload));
            } catch (NumberFormatException error) {
                Log.w(TAG, "failed to parse Pimax IPD payload: " + payload, error);
            }
        } else if (type == PMX_EVENT_TYPE_LENS_CHANGED) {
            Log.i(TAG, "Pimax lens change event received: value=" + value);
        }
    }

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

    private void requestNativeShutdown(String reason) {
        nativeShutdownRequested = true;
        try {
            nativeRequestShutdown();
            Log.i(TAG, "requested native shutdown: " + reason);
        } catch (UnsatisfiedLinkError error) {
            Log.w(TAG, "native shutdown hook unavailable: " + reason, error);
        } catch (RuntimeException error) {
            Log.w(TAG, "native shutdown hook failed: " + reason, error);
        }
    }

    private void resetNativeShutdown(String reason) {
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
