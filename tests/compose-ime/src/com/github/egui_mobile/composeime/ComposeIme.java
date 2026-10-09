package com.github.egui_mobile.composeime;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.inputmethodservice.InputMethodService;
import android.util.Log;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.inputmethod.InputConnection;
import android.widget.TextView;

/** A keyboard without keys: adb broadcasts drive its InputConnection the way a typing IME would. */
public class ComposeIme extends InputMethodService {
    static final String TAG = "ComposeIme";
    static final String PREFIX = "egui.ime.";

    private final BroadcastReceiver receiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent.getAction();
            String text = intent.getStringExtra("text");
            if (text == null) text = "";
            InputConnection ic = getCurrentInputConnection();
            boolean ok = false;
            if (ic != null && action != null) {
                switch (action.substring(PREFIX.length())) {
                    case "COMPOSE":
                        ok = ic.setComposingText(text, 1);
                        break;
                    case "COMMIT":
                        ok = ic.commitText(text, 1);
                        break;
                    case "FINISH":
                        ok = ic.finishComposingText();
                        break;
                    case "ENTER":
                        ok = ic.sendKeyEvent(new KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER))
                                && ic.sendKeyEvent(new KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER));
                        break;
                    default:
                        break;
                }
            }
            Log.i(TAG, action + " \"" + text + "\" connected=" + (ic != null) + " ok=" + ok);
        }
    };

    @Override
    public void onCreate() {
        super.onCreate();
        IntentFilter filter = new IntentFilter();
        for (String name : new String[] {"COMPOSE", "COMMIT", "FINISH", "ENTER"}) {
            filter.addAction(PREFIX + name);
        }
        registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED);
    }

    @Override
    public void onDestroy() {
        unregisterReceiver(receiver);
        super.onDestroy();
    }

    @Override
    public boolean onEvaluateFullscreenMode() {
        return false;
    }

    // Shown even when the emulator reports a hardware keyboard.
    @Override
    public boolean onEvaluateInputViewShown() {
        super.onEvaluateInputViewShown();
        return true;
    }

    @Override
    public View onCreateInputView() {
        // Keyboard-sized, so runtimes that measure the IME inset see a keyboard.
        TextView view = new TextView(this);
        view.setText("compose-ime: adb shell am broadcast -a egui.ime.COMPOSE --es text ...");
        view.setGravity(Gravity.CENTER);
        view.setMinHeight((int) (280 * getResources().getDisplayMetrics().density));
        view.setBackgroundColor(0xFF202124);
        view.setTextColor(0xFFE8EAED);
        return view;
    }
}
