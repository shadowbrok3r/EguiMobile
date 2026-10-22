package com.github.egui_mobile;

import android.app.Service;
import android.content.Intent;
import android.os.*;
import org.json.*;
import java.io.*;
import java.net.*;
import java.util.*;
import java.util.concurrent.*;

/** Store-owned update checks. Only metadata for the calling UID's package leaves the store. */
public final class StoreUpdateService extends Service {
    private final ExecutorService worker = new ThreadPoolExecutor(1,1,0L,TimeUnit.MILLISECONDS,
        new ArrayBlockingQueue<>(16),new ThreadPoolExecutor.AbortPolicy());
    private JSONArray cached;
    private String cachedConfig="";
    private long cacheTime;
    private final Messenger messenger=new Messenger(new Handler(Looper.getMainLooper(),message->{
        if(message.what!=1 || message.replyTo==null)return true;
        String pkg=message.getData().getString("package","");
        String[] owned=getPackageManager().getPackagesForUid(message.sendingUid);
        if(owned==null || !Arrays.asList(owned).contains(pkg))return true;
        Messenger reply=message.replyTo;
        boolean force=message.getData().getBoolean("force",false);
        try { worker.execute(()->{
            Bundle result=new Bundle();
            try {
                JSONArray apps=catalog(force);
                for(int i=0;i<apps.length();i++) {
                    JSONObject app=apps.getJSONObject(i);
                    if(!pkg.equals(app.optString("package")))continue;
                    JSONObject latest=app.optJSONObject("latest");
                    if(latest!=null){result.putBoolean("known",true);result.putLong("version_code",latest.optLong("version_code"));result.putString("version_name",latest.optString("version_name"));}
                    break;
                }
            } catch(Exception e) { android.util.Log.w("AppStoreUpdates","Lookup failed: "+e.getClass().getSimpleName()); }
            try {Message out=Message.obtain(null,1);out.setData(result);reply.send(out);}catch(Exception ignored){}
        }); } catch(RejectedExecutionException ignored) {}
        return true;
    }));
    public IBinder onBind(Intent intent){return messenger.getBinder();}
    public void onDestroy(){worker.shutdownNow();super.onDestroy();}
    private static byte[] read(InputStream in,int limit)throws IOException {
        try(InputStream input=in;ByteArrayOutputStream out=new ByteArrayOutputStream()){
            byte[] buf=new byte[8192];int n;while((n=input.read(buf))!=-1){if(out.size()+n>limit)throw new IOException("too large");out.write(buf,0,n);}return out.toByteArray();
        }
    }
    private JSONArray catalog(boolean force)throws Exception {
        String raw=new String(read(new FileInputStream(new File(getFilesDir(),"appstore/config.json")),65536),"UTF-8");
        if(!force && cached!=null && raw.equals(cachedConfig) && SystemClock.elapsedRealtime()-cacheTime<300000)return cached;
        JSONArray apps=fetchCatalog(new JSONObject(raw));
        cached=apps;cachedConfig=raw;cacheTime=SystemClock.elapsedRealtime();return apps;
    }
    static JSONObject config(android.content.Context context)throws Exception {
        return new JSONObject(new String(read(new FileInputStream(new File(context.getFilesDir(),"appstore/config.json")),65536),"UTF-8"));
    }
    static JSONArray fetchCatalog(JSONObject config)throws Exception {
        String base=config.getString("server_url").replaceAll("/+$","");
        URL url=new URL(base+"/api/apps");
        if(!url.getProtocol().equals("https") && !(url.getProtocol().equals("http") && (url.getHost().equals("127.0.0.1") || url.getHost().equals("localhost") || url.getHost().equals("10.0.2.2"))))throw new IOException("HTTPS required");
        HttpURLConnection conn=(HttpURLConnection)url.openConnection();
        try {
            conn.setInstanceFollowRedirects(false);conn.setConnectTimeout(8000);conn.setReadTimeout(8000);
            conn.setRequestProperty("Accept","application/json");String key=config.optString("api_key","");
            conn.setRequestProperty("User-Agent","AppStore/0.3 Android");
            if(!key.isEmpty())conn.setRequestProperty("x-api-key",key);
            if(conn.getResponseCode()!=200)throw new IOException("store unavailable");
            JSONArray apps=new JSONObject(new String(read(conn.getInputStream(),2*1024*1024),"UTF-8")).getJSONArray("apps");
            return apps;
        } finally {conn.disconnect();}
    }
}
