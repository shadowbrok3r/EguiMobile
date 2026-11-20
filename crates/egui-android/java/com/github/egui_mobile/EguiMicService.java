package com.github.egui_mobile;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.MediaRecorder;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.util.Log;

/**
 * Microphone-typed foreground service that records with AudioRecord and hands the PCM to Rust.
 *
 * Capture lives here rather than in the activity because Android silences the microphone of an
 * app with no visible activity unless a foreground service of this type is running — and because
 * the activity is destroyed when its task is swiped away, while a recording should carry on.
 *
 * The service outlives the capture on purpose: after {@link #hold} it records nothing but keeps
 * the process in the foreground and awake, for work on the recording that is still running.
 * Only Rust ends it, with {@link #release}.
 */
public class EguiMicService extends Service {
    private static final String TAG = "EguiMic";

    public static final String ACTION_START = "com.github.egui_mobile.action.MIC_START";
    /** The notification's Stop button. */
    public static final String ACTION_STOP = "com.github.egui_mobile.action.MIC_STOP";

    private static final String CHANNEL_ID = "egui_mic";
    private static final int NOTIFICATION_ID = 8;
    private static final int SDK_R = 30;
    /** Rates tried in order after the requested one; 44.1 kHz is the only one Android guarantees. */
    private static final int[] FALLBACK_RATES = {48000, 44100};

    /** The live instance, for the static calls Rust makes from its worker threads. */
    private static volatile EguiMicService running;
    private static final Handler MAIN = new Handler(Looper.getMainLooper());

    private volatile boolean capturing;
    private AudioRecord recorder;
    private Thread reader;
    private PowerManager.WakeLock wakeLock;
    private String title = "Recording";
    private String text = "";
    private long captureStartedAt;
    /** The newest start this instance has been handed; see {@link #shutdown}. */
    private int lastStartId;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onCreate() {
        super.onCreate();
        running = this;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        lastStartId = startId;
        String action = intent != null ? intent.getAction() : null;
        if (ACTION_STOP.equals(action)) {
            stopCapture();
            // The row must stop counting and offering Stop now, not when Rust next speaks.
            goForeground();
            try {
                nativeMicStopRequested();
            } catch (Throwable t) {
                // Nobody is left to finish the recording or to release this service.
                Log.e(TAG, "nativeMicStopRequested unavailable: " + t);
                shutdown();
            }
            return START_NOT_STICKY;
        }

        title = extra(intent, "title", title);
        text = extra(intent, "text", text);
        if (!goForeground()) {
            stopSelf();
            return START_NOT_STICKY;
        }
        if (ACTION_START.equals(action) && !capturing) {
            startCapture(intent.getIntExtra("rate", 16000), intent.getIntExtra("source", MediaRecorder.AudioSource.VOICE_RECOGNITION));
        }
        // Restarting with no intent would call into a native lib the activity may not have loaded.
        return START_NOT_STICKY;
    }

    /** Stop capturing but stay in the foreground under a new notification. Any thread. */
    public static void hold(final String title, final String text) {
        MAIN.post(() -> {
            EguiMicService service = running;
            if (service == null) return;
            service.stopCapture();
            service.title = title;
            service.text = text;
            service.goForeground();
        });
    }

    /** Replace the notification's text. Any thread. */
    public static void text(final String title, final String text) {
        MAIN.post(() -> {
            EguiMicService service = running;
            if (service == null) return;
            service.title = title;
            service.text = text;
            service.goForeground();
        });
    }

    /** End the service and its notification. Any thread. */
    public static void release() {
        MAIN.post(() -> {
            EguiMicService service = running;
            if (service != null) service.shutdown();
        });
    }

    private void shutdown() {
        stopCapture();
        stopForeground(STOP_FOREGROUND_REMOVE);
        // Not stopSelf(): a release posted a moment ago can run after a new recording's
        // startForegroundService() was accepted, and stopping a service that still owes
        // startForeground() gets the whole app killed. With the id, the system declines when a
        // newer start is on its way, and that start then brings this back to the foreground.
        if (!stopSelfResult(lastStartId)) {
            Log.i(TAG, "a newer start is pending; staying up for it");
        }
    }

    private void startCapture(int requestedRate, int source) {
        AudioRecord record = null;
        int rate = requestedRate;
        String failure = "no supported sample rate";
        int[] rates = new int[FALLBACK_RATES.length + 1];
        rates[0] = requestedRate;
        System.arraycopy(FALLBACK_RATES, 0, rates, 1, FALLBACK_RATES.length);
        for (int candidate : rates) {
            try {
                int min = AudioRecord.getMinBufferSize(candidate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT);
                if (min <= 0) continue;
                // A second of slack: a reader stalled behind a slow sink drops nothing for that long.
                AudioRecord attempt = new AudioRecord(source, candidate, AudioFormat.CHANNEL_IN_MONO,
                        AudioFormat.ENCODING_PCM_16BIT, Math.max(min, candidate * 2));
                if (attempt.getState() == AudioRecord.STATE_INITIALIZED) {
                    record = attempt;
                    rate = candidate;
                    break;
                }
                attempt.release();
            } catch (Exception e) {
                failure = e.toString();
            }
        }
        if (record == null) {
            fail("could not open the microphone: " + failure);
            return;
        }

        try {
            record.startRecording();
        } catch (Exception e) {
            record.release();
            fail("could not start recording: " + e);
            return;
        }
        // Another app holding the microphone, or a missing grant, shows up here and not as an exception.
        if (record.getRecordingState() != AudioRecord.RECORDSTATE_RECORDING) {
            record.release();
            fail("the microphone is in use or not permitted");
            return;
        }

        recorder = record;
        capturing = true;
        captureStartedAt = System.currentTimeMillis();
        goForeground();
        final AudioRecord source_ = record;
        final int rate_ = rate;
        reader = new Thread(() -> readLoop(source_, rate_), "egui-mic");
        reader.start();
    }

    private void readLoop(AudioRecord record, int rate) {
        try {
            nativeMicStarted(rate);
            // 100 ms per callback: frequent enough for a level meter, rare enough to be cheap.
            short[] buffer = new short[rate / 10];
            while (capturing) {
                int n = record.read(buffer, 0, buffer.length);
                if (n > 0) {
                    nativeMicPcm(buffer, n);
                } else if (n < 0) {
                    if (capturing) {
                        nativeMicFailed("microphone read failed (" + n + ")");
                        // Nothing is being captured any more; let the recorder go.
                        MAIN.post(this::stopCapture);
                    }
                    break;
                }
            }
        } catch (Throwable t) {
            // The native library is only loaded once the activity has run.
            Log.e(TAG, "capture loop ended: " + t);
        } finally {
            // However the loop ended, Rust waits on this before it lets another capture start.
            try { nativeMicStopped(); } catch (Throwable ignored) {}
        }
    }

    private void stopCapture() {
        if (!capturing && recorder == null) return;
        capturing = false;
        AudioRecord record = recorder;
        recorder = null;
        if (record != null) {
            // stop() is what unblocks a read in progress.
            try { record.stop(); } catch (Exception ignored) {}
        }
        Thread thread = reader;
        reader = null;
        if (thread != null && thread != Thread.currentThread()) {
            try { thread.join(1000); } catch (InterruptedException ignored) {}
        }
        if (record != null) record.release();
    }

    private void fail(String reason) {
        Log.e(TAG, reason);
        try { nativeMicFailed(reason); } catch (Throwable ignored) {}
        shutdown();
    }

    private boolean goForeground() {
        NotificationManager manager = (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
        // IMPORTANCE_LOW: an ongoing status row, not something that should buzz.
        NotificationChannel channel = new NotificationChannel(CHANNEL_ID, "Recording", NotificationManager.IMPORTANCE_LOW);
        channel.setShowBadge(false);
        manager.createNotificationChannel(channel);

        // A foreground notification without a small icon kills the process asynchronously, past
        // any try/catch here; see EguiVpnService.
        int icon = getApplicationInfo().icon;
        if (icon == 0) icon = android.R.drawable.sym_def_app_icon;

        Notification.Builder builder = new Notification.Builder(this, CHANNEL_ID)
                .setContentTitle(title)
                .setContentText(text)
                .setSmallIcon(icon)
                .setContentIntent(launchIntent())
                .setOngoing(true)
                .setOnlyAlertOnce(true);
        if (capturing) {
            Intent stop = new Intent(this, EguiMicService.class).setAction(ACTION_STOP);
            PendingIntent stopIntent = PendingIntent.getService(
                    this, 0, stop, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
            // The row counts the recording's length by itself, with no updates from the app.
            builder.setUsesChronometer(true).setWhen(captureStartedAt).setShowWhen(true)
                    .addAction(new Notification.Action.Builder(null, "Stop", stopIntent).build());
        } else {
            builder.setShowWhen(false);
        }

        try {
            if (Build.VERSION.SDK_INT >= SDK_R) {
                startForeground(NOTIFICATION_ID, builder.build(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
            } else {
                startForeground(NOTIFICATION_ID, builder.build());
            }
        } catch (Exception e) {
            // Android 14+ refuses the microphone type without RECORD_AUDIO, or from the background.
            Log.e(TAG, "startForeground failed: " + e);
            try { nativeMicFailed("Android refused the recording service: " + e); } catch (Throwable ignored) {}
            return false;
        }

        // The screen going off must not suspend the capture thread or the work that follows it.
        if (wakeLock == null) {
            PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
            wakeLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, getPackageName() + ":mic");
            wakeLock.setReferenceCounted(false);
        }
        try { wakeLock.acquire(); } catch (Exception e) { Log.w(TAG, "no wake lock: " + e); }
        return true;
    }

    private PendingIntent launchIntent() {
        Intent launch = getPackageManager().getLaunchIntentForPackage(getPackageName());
        return PendingIntent.getActivity(
                this, 0, launch, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    private static String extra(Intent intent, String key, String fallback) {
        String value = intent != null ? intent.getStringExtra(key) : null;
        return value != null ? value : fallback;
    }

    @Override
    public void onDestroy() {
        stopCapture();
        if (wakeLock != null && wakeLock.isHeld()) {
            try { wakeLock.release(); } catch (Exception ignored) {}
        }
        if (running == this) running = null;
        super.onDestroy();
    }

    /** Implemented in Rust (egui-android `mic`); registered by `mic::register_natives`. */
    private static native void nativeMicStarted(int rate);
    private static native void nativeMicPcm(short[] samples, int count);
    private static native void nativeMicStopped();
    private static native void nativeMicFailed(String reason);
    private static native void nativeMicStopRequested();
}
