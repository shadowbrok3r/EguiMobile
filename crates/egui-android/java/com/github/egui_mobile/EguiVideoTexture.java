package com.github.egui_mobile;

import android.graphics.SurfaceTexture;
import android.media.MediaPlayer;
import android.media.AudioAttributes;
import android.media.MediaMetadataRetriever;
import android.os.Handler;
import android.os.Looper;
import android.view.Surface;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/** External-OES surface. Decoder buffers stay on the GPU and egui draws only a textured quad. */
public final class EguiVideoTexture {
    private final SurfaceTexture texture;
    private final Surface surface;
    private final float[] transform = new float[16];
    private final AtomicBoolean available = new AtomicBoolean();
    private final Handler main = new Handler(Looper.getMainLooper());
    private MediaPlayer player;
    private volatile boolean ready, closed, desiredPlaying = true;
    private volatile long position, duration;
    private volatile int width, height;
    private volatile long frameCount, fpsMilli = 30000;
    private volatile boolean hasAudio;
    private volatile String error = "";
    private boolean looping = true, muted;
    private boolean loggedFrame;
    private long pendingSeek = -1;

    public EguiVideoTexture(int textureId) {
        texture = new SurfaceTexture(textureId);
        texture.setDefaultBufferSize(1920,1080);
        surface = new Surface(texture);
        for (int i=0;i<16;i++) transform[i] = i%5==0 ? 1 : 0;
        texture.setOnFrameAvailableListener(t -> available.set(true), main);
    }

    public Surface surface() { return surface; }
    public void bufferSize(int width,int height) { texture.setDefaultBufferSize(Math.max(1,width),Math.max(1,height)); }

    /** Called only on the GL render thread. */
    public float[] update() {
        if (!closed && available.getAndSet(false)) {
            texture.updateTexImage();
            texture.getTransformMatrix(transform);
            if (!loggedFrame) { android.util.Log.i("EguiVideo", "First GPU frame at " + texture.getTimestamp()); loggedFrame=true; }
        }
        return transform;
    }

    public void open(String path) {
        main.post(() -> {
            if (closed) return;
            try {
                player = new MediaPlayer();
                player.setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA).setContentType(AudioAttributes.CONTENT_TYPE_MOVIE).build());
                player.setSurface(surface);
                player.setDataSource(path);
                player.setLooping(looping);
                player.setVolume(muted ? 0 : 1, muted ? 0 : 1);
                player.setOnVideoSizeChangedListener((p,w,h) -> { width=w; height=h; bufferSize(w,h); });
                player.setOnPreparedListener(p -> {
                    if (closed) return;
                    ready=true; duration=p.getDuration(); width=p.getVideoWidth(); height=p.getVideoHeight();
                    for(MediaPlayer.TrackInfo track:p.getTrackInfo()) if(track.getTrackType()==MediaPlayer.TrackInfo.MEDIA_TRACK_TYPE_AUDIO) hasAudio=true;
                    if (pendingSeek >= 0) { p.seekTo(pendingSeek,MediaPlayer.SEEK_CLOSEST); pendingSeek=-1; }
                    if (desiredPlaying) p.start();
                    main.post(clock);
                });
                player.setOnCompletionListener(p -> desiredPlaying=false);
                player.setOnErrorListener((p,what,extra) -> { error="Android video decoder error " + what + "/" + extra; ready=false; return true; });
                player.prepareAsync();
            } catch (Exception e) { error=e.toString(); }
        });
        new Thread(() -> {
            try(MediaMetadataRetriever metadata=new MediaMetadataRetriever()) {
                metadata.setDataSource(path);
                String count=metadata.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_FRAME_COUNT);
                String length=metadata.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION);
                String rate=metadata.extractMetadata(MediaMetadataRetriever.METADATA_KEY_CAPTURE_FRAMERATE);
                long frames=count==null?0:Long.parseLong(count), ms=length==null?0:Long.parseLong(length);
                if(!closed){frameCount=frames;if(frames>0&&ms>0)fpsMilli=frames*1000000/ms;else if(rate!=null)fpsMilli=(long)(Double.parseDouble(rate)*1000);}
            } catch(Exception ignored) { }
        },"video-metadata").start();
    }

    private final Runnable clock = new Runnable() {
        @Override public void run() {
            if (closed || player==null) return;
            try { if(ready) position=player.getCurrentPosition(); }
            catch(IllegalStateException ignored) { }
            main.postDelayed(this,40);
        }
    };

    public void playing(boolean value) {
        desiredPlaying=value;
        main.post(() -> { try {if(ready&&player!=null) {if(value) player.start();else player.pause();}} catch(IllegalStateException e){error=e.toString();} });
    }
    public void seek(long ms) {
        position=ms;
        main.post(() -> {try {if(ready&&player!=null) player.seekTo(Math.max(0,ms),MediaPlayer.SEEK_CLOSEST);else pendingSeek=Math.max(0,ms);}catch(IllegalStateException e){error=e.toString();}});
    }
    public void muted(boolean value) { muted=value; main.post(() -> {if(player!=null) try{player.setVolume(value?0:1,value?0:1);}catch(IllegalStateException ignored){}}); }
    public void looping(boolean value) { looping=value; main.post(() -> {if(player!=null) try{player.setLooping(value);}catch(IllegalStateException ignored){}}); }
    public long[] status() {return new long[]{position,duration,width,height,ready?1:0,desiredPlaying?1:0,fpsMilli,frameCount,hasAudio?1:0};}
    public String error() {return error;}

    public void close() {
        closed=true;
        CountDownLatch stopped=new CountDownLatch(1);
        Runnable stop=() -> {main.removeCallbacks(clock);if(player!=null){player.release();player=null;}stopped.countDown();};
        if(Looper.myLooper()==Looper.getMainLooper()) stop.run();
        else {main.post(stop);try{stopped.await(2,TimeUnit.SECONDS);}catch(InterruptedException e){Thread.currentThread().interrupt();}}
        surface.release(); texture.release();
    }
}
