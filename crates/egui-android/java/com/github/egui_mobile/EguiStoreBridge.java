package com.github.egui_mobile;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.*;
import android.os.*;
import android.widget.Toast;

/** Shared client: asks the installed store about this package, never receives its credentials. */
final class EguiStoreBridge {
    static final String STORE = "com.kingsofalchemy.appstore";
    private final Activity activity;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private boolean checking;
    private ServiceConnection active;
    private long checkedAt;
    EguiStoreBridge(Activity activity) { this.activity = activity; }

    boolean available() {
        if (STORE.equals(activity.getPackageName())) return false;
        try { return activity.getPackageManager().getServiceInfo(
            new ComponentName(STORE, "com.github.egui_mobile.StoreUpdateService"), 0).enabled; }
        catch (Exception e) { return false; }
    }
    void open() {
        try {
            activity.startActivity(new Intent().setClassName(STORE, "com.github.egui_mobile.EguiNativeActivity")
                .setAction("com.kingsofalchemy.appstore.OPEN_APP")
                .putExtra("package", activity.getPackageName())
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP | Intent.FLAG_ACTIVITY_CLEAR_TOP));
        } catch (Exception e) { Toast.makeText(activity, "App Store is not installed", Toast.LENGTH_SHORT).show(); }
    }
    void check(boolean force) {
        if (checking || STORE.equals(activity.getPackageName())) return;
        long now = SystemClock.elapsedRealtime();
        if (!force && checkedAt > 0 && now - checkedAt < 6 * 60 * 60 * 1000L) return;
        checking = true;
        final ServiceConnection[] connection = new ServiceConnection[1];
        final boolean[] bound = { false };
        Runnable finish = () -> { if (bound[0]) { try {activity.unbindService(connection[0]);} catch(Exception ignored){} bound[0]=false;} if(active==connection[0]){active=null;checking=false;} };
        Messenger reply = new Messenger(new Handler(Looper.getMainLooper(), message -> {
            finish.run();
            if (activity.isFinishing() || activity.isDestroyed()) return true;
            Bundle data=message.getData();
            try {
                android.content.pm.PackageInfo info=activity.getPackageManager().getPackageInfo(activity.getPackageName(),0);
                long installed=Build.VERSION.SDK_INT>=28?info.getLongVersionCode():info.versionCode;
                long version=data.getLong("version_code",0);
                if (version>installed) {
                    String key="offer_"+version;
                    android.content.SharedPreferences prefs=activity.getPreferences(Context.MODE_PRIVATE);
                    if (force || System.currentTimeMillis()-prefs.getLong(key,0)>86400000L) {
                        prefs.edit().putLong(key,System.currentTimeMillis()).apply();
                        new AlertDialog.Builder(activity).setTitle("Update available")
                            .setMessage("Version "+data.getString("version_name","")+" is available in App Store.")
                            .setPositiveButton("Open App Store",(d,w)->open()).setNegativeButton("Later",null).show();
                    }
                } else if(force) {
                    if(data.getBoolean("known",false)) Toast.makeText(activity,"Up to date",Toast.LENGTH_SHORT).show();
                    else open();
                }
            } catch(Exception ignored) { if(force) open(); }
            return true;
        }));
        connection[0]=new ServiceConnection() {
            public void onServiceConnected(ComponentName name, IBinder binder) {
                try { Message m=Message.obtain(null,1);Bundle b=new Bundle();b.putString("package",activity.getPackageName());b.putBoolean("force",force);m.setData(b);m.replyTo=reply;new Messenger(binder).send(m); }
                catch(Exception e){ finish.run(); }
            }
            public void onServiceDisconnected(ComponentName name){finish.run();}
            public void onNullBinding(ComponentName name){finish.run();}
            public void onBindingDied(ComponentName name){finish.run();}
        };
        try {
            active=connection[0];
            bound[0]=activity.bindService(new Intent().setClassName(STORE,"com.github.egui_mobile.StoreUpdateService"),connection[0],Context.BIND_AUTO_CREATE);
            if(bound[0]) {checkedAt=now;handler.postDelayed(finish,25000);} else {checking=false;if(force)open();}
        } catch(Exception e){checking=false;}
    }
}
