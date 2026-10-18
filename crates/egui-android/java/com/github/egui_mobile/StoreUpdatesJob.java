package com.github.egui_mobile;

import android.app.*;
import android.app.job.*;
import android.content.*;
import android.content.pm.PackageInfo;
import android.os.Build;
import org.json.*;

/** A periodic check covers every installed catalog app, including older APKs without the bridge. */
public final class StoreUpdatesJob extends JobService {
    private static final int JOB=8311;
    private volatile JobParameters active;
    public static void schedule(Context context) {
        if(!EguiStoreBridge.STORE.equals(context.getPackageName()))return;
        try {
            JSONObject cfg=StoreUpdateService.config(context);
            JobScheduler scheduler=(JobScheduler)context.getSystemService(Context.JOB_SCHEDULER_SERVICE);
            if(!cfg.optBoolean("background_updates",true)){scheduler.cancel(JOB);return;}
            if(scheduler.getPendingJob(JOB)!=null)return;
            scheduler.schedule(new JobInfo.Builder(JOB,new ComponentName(context,StoreUpdatesJob.class))
                .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY).setRequiresBatteryNotLow(true)
                .setPeriodic(6*60*60*1000L).setPersisted(true).build());
        }catch(Exception e){android.util.Log.w("AppStoreUpdates","Scheduling failed: "+e.getClass().getSimpleName());}
    }
    public boolean onStartJob(JobParameters params){
        active=params;
        new Thread(()->{
            try {
                JSONObject cfg=StoreUpdateService.config(this);
                if(cfg.optBoolean("background_updates",true)) {
                    JSONArray apps=StoreUpdateService.fetchCatalog(cfg);
                    int count=0;
                    for(int i=0;i<apps.length();i++){
                        JSONObject app=apps.getJSONObject(i),latest=app.optJSONObject("latest");
                        if(latest==null)continue;
                        try{
                            PackageInfo installed=getPackageManager().getPackageInfo(app.getString("package"),0);
                            long code=Build.VERSION.SDK_INT>=28?installed.getLongVersionCode():installed.versionCode;
                            if(latest.optLong("version_code")>code)count++;
                        }catch(Exception ignored){}
                    }
                    if(active!=params)return;
                    NotificationManager manager=(NotificationManager)getSystemService(NOTIFICATION_SERVICE);
                    if(count>0) {
                        manager.createNotificationChannel(new NotificationChannel("app_updates","App updates",NotificationManager.IMPORTANCE_DEFAULT));
                        Intent intent=new Intent().setClassName(EguiStoreBridge.STORE,"com.github.egui_mobile.EguiNativeActivity");
                        PendingIntent open=PendingIntent.getActivity(this,JOB,intent,PendingIntent.FLAG_UPDATE_CURRENT|PendingIntent.FLAG_IMMUTABLE);
                        Notification notice=new Notification.Builder(this,"app_updates").setSmallIcon(android.R.drawable.stat_sys_download_done)
                            .setContentTitle(count==1?"1 app update available":count+" app updates available")
                            .setContentText("Open App Store to update your apps.").setContentIntent(open).setAutoCancel(true).setOnlyAlertOnce(true).build();
                        manager.notify(JOB,notice);
                    }else manager.cancel(JOB);
                }
            }catch(Exception e){android.util.Log.w("AppStoreUpdates","Check failed: "+e.getClass().getSimpleName());} finally{if(active==params){active=null;jobFinished(params,false);}}
        },"store-update-check").start();
        return true;
    }
    public boolean onStopJob(JobParameters params){if(active==params)active=null;return true;}
}
