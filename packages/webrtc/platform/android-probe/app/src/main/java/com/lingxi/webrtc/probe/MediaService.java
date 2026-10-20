package com.lingxi.webrtc.probe;

import android.app.*;
import android.content.*;
import android.content.pm.ServiceInfo;
import android.graphics.PixelFormat;
import android.graphics.ImageFormat;
import android.hardware.camera2.*;
import android.hardware.display.*;
import android.media.*;
import android.media.audiofx.*;
import android.media.projection.*;
import android.os.*;
import android.os.Process;
import android.util.Range;
import android.view.Surface;
import java.nio.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicBoolean;
import org.json.JSONObject;

/** Native devices only. Rust owns all APM, Opus/VP8 and WebRTC protocols. */
public final class MediaService extends Service {
    public static final String START="start", STOP="stop", PROJECT="project";
    final class LocalBinder extends Binder { MediaService service(){return MediaService.this;} }
    private final LocalBinder binder=new LocalBinder();
    private final AtomicBoolean running=new AtomicBoolean();
    private final AtomicBoolean stopping=new AtomicBoolean();
    private volatile boolean destroyed;
    private final Object nativeLock=new Object();
    private volatile long handle;
    private volatile String failure="Starting devices";
    private HandlerThread deviceThread;
    private Handler deviceHandler;
    private AudioRecord recorder;
    private AudioTrack player;
    private Thread captureThread,renderThread;
    private CameraDevice camera;
    private CameraCaptureSession cameraSession;
    private ImageReader cameraImages,screenImages;
    private MediaProjection projection;
    private MediaProjection.Callback projectionCallback;
    private PowerManager powerManager;
    private PowerManager.OnThermalStatusChangedListener thermalListener;
    private volatile int thermalStatus;
    private final ThermalVideoPolicy thermalPolicy=new ThermalVideoPolicy();
    private volatile int videoFps=30;
    private CaptureRequest.Builder cameraRequest;
    private Range<Integer>[] cameraFpsRanges;
    private long lastCamera;
    private volatile String stopReason="Media stopped; tap Start to reconnect";
    private final Runnable healthCheck=new Runnable(){public void run(){
        // Transport failure must release devices even while Activity polling is
        // suspended in the background during a cross-app screen share.
        updateThermalVideo();
        if(running.get())stats();
        if(!destroyed)deviceHandler.postDelayed(this,1000);
    }};
    private volatile boolean sharingScreen;
    private VirtualDisplay display;
    private long lastScreen;
    volatile long screenCallbacks,screenSubmitted,screenAgeNs,screenRateLimited,screenInvalidTimestamp;
    volatile int screenSubmitResult;
    private int bits=1|2|4|16;
    private AcousticEchoCanceler aec;
    private NoiseSuppressor ns;
    private AutomaticGainControl agc;

    @Override public void onCreate(){
        super.onCreate();deviceThread=new HandlerThread("webrtc-native-devices");deviceThread.start();deviceHandler=new Handler(deviceThread.getLooper());
        deviceHandler.post(healthCheck);
        powerManager=getSystemService(PowerManager.class);
        if(Build.VERSION.SDK_INT>=29){
            thermalStatus=powerManager.getCurrentThermalStatus();
            thermalListener=status->{
                thermalStatus=status;
                if(status>=PowerManager.THERMAL_STATUS_SEVERE && running.get()){
                    stopReason="Device is too warm. Media stopped; let it cool before restarting.";failure=stopReason;
                    stopMedia();stopForeground(STOP_FOREGROUND_REMOVE);
                }
                deviceHandler.post(()->updateThermalVideo());
                recordLifecycle();
            };
            powerManager.addThermalStatusListener(getMainExecutor(),thermalListener);
        }
    }
    private void updateThermalVideo(){
        int next=thermalPolicy.update(thermalStatus,SystemClock.elapsedRealtime());
        if(next==videoFps)return;
        videoFps=next;lastCamera=0;lastScreen=0;
        applyCameraRate();recordLifecycle();
    }
    private void applyCameraRate(){
        if(cameraRequest==null || cameraSession==null || !running.get())return;
        Range<Integer> selected=null;
        if(cameraFpsRanges!=null)for(Range<Integer> range:cameraFpsRanges){
            if(range.getUpper()>videoFps)continue;
            if(selected==null || range.getUpper()>selected.getUpper() ||
                    (range.getUpper().equals(selected.getUpper()) && range.getLower()>selected.getLower()))selected=range;
        }
        if(selected!=null)cameraRequest.set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE,selected);
        // Some cameras expose no <=15 fps range. Admission still bounds work
        // before entering shared conversion/encoding in that case.
        try{cameraSession.setRepeatingRequest(cameraRequest.build(),null,deviceHandler);}
        catch(CameraAccessException | IllegalStateException error){failure="Camera rate update failed";}
    }
    private void recordLifecycle(){
        deviceHandler.post(()->{
            try(java.io.FileOutputStream out=openFileOutput("webrtc-lifecycle-receipt.json",MODE_PRIVATE)){
                JSONObject receipt=new JSONObject().put("recordedAt",System.currentTimeMillis()/1000.).put("status",failure).put("thermalStatus",thermalStatus).put("videoTargetFPS",videoFps).put("running",running.get()).put("stopping",stopping.get()).put("sharingScreen",sharingScreen);
                out.write(receipt.toString().getBytes(java.nio.charset.StandardCharsets.UTF_8));
            }catch(Exception ignored){}
        });
    }
    @Override public IBinder onBind(Intent intent){return binder;}
    @Override public int onStartCommand(Intent intent,int flags,int id){
        if(intent==null)return START_NOT_STICKY;
        if(STOP.equals(intent.getAction())){stopMedia();stopForeground(STOP_FOREGROUND_REMOVE);stopSelf();return START_NOT_STICKY;}
        boolean screen=PROJECT.equals(intent.getAction());
        boolean needsProjectionType=screen || sharingScreen;
        NotificationManager notifications=getSystemService(NotificationManager.class);
        notifications.createNotificationChannel(new NotificationChannel("webrtc-probe","WebRTC device test",NotificationManager.IMPORTANCE_LOW));
        PendingIntent stop=PendingIntent.getService(this,1,new Intent(this,MediaService.class).setAction(STOP),PendingIntent.FLAG_UPDATE_CURRENT|PendingIntent.FLAG_IMMUTABLE);
        Notification notice=new Notification.Builder(this,"webrtc-probe").setSmallIcon(android.R.drawable.ic_btn_speak_now).setContentTitle("LingXi WebRTC device test").setContentText("Microphone and camera active; Stop ends local testing").setOngoing(true).addAction(new Notification.Action.Builder(null,"Stop",stop).build()).build();
        if(Build.VERSION.SDK_INT>=30) startForeground(7,notice,ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE|ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA|(needsProjectionType?ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION:0));
        else if(Build.VERSION.SDK_INT>=29) startForeground(7,notice,needsProjectionType?ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION:0);
        else startForeground(7,notice);
        if(thermalStatus>=PowerManager.THERMAL_STATUS_SEVERE){
            stopReason="Device is too warm. Let it cool before starting media.";failure=stopReason;
            stopMedia();recordLifecycle();stopForeground(STOP_FOREGROUND_REMOVE);stopSelf(id);return START_NOT_STICKY;
        }
        if(screen){
            Intent consent=intent.getParcelableExtra("consent");
            if(consent!=null)deviceHandler.post(()->startProjection(intent.getIntExtra("result",0),consent));
        }else if(!stopping.get() && running.compareAndSet(false,true)){
            bits=1|2|4|16;
            stopReason="Media stopped; tap Start to reconnect";
            String endpoint=intent.getStringExtra("endpoint");
            deviceHandler.post(()->startDevices(endpoint==null?"192.168.0.2:18080":endpoint,intent.getStringExtra("iceConfig")));
        }
        return START_NOT_STICKY;
    }
    private void startDevices(String endpoint,String iceJson){
        try {
            long created=NativeBridge.open(endpoint,"local-webrtc-test-token",48000,48000,iceJson==null?null:iceJson.getBytes(java.nio.charset.StandardCharsets.UTF_8));
            if(created==0)throw new IllegalStateException("Rust initialization failed");
            synchronized(nativeLock){handle=created;NativeBridge.controls(handle,bits);}
            int sampleFormat=AudioFormat.ENCODING_PCM_FLOAT;
            AudioFormat format=new AudioFormat.Builder().setSampleRate(48000).setEncoding(sampleFormat).setChannelMask(AudioFormat.CHANNEL_IN_MONO).build();
            AudioManager manager=getSystemService(AudioManager.class);
            boolean unprocessed="true".equals(manager.getProperty(AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED));
            int buffer=Math.max(AudioRecord.getMinBufferSize(48000,AudioFormat.CHANNEL_IN_MONO,sampleFormat),480*4*8);
            recorder=new AudioRecord.Builder().setAudioSource(unprocessed?MediaRecorder.AudioSource.UNPROCESSED:MediaRecorder.AudioSource.VOICE_RECOGNITION).setAudioFormat(format).setBufferSizeInBytes(buffer).build();
            aec=AcousticEchoCanceler.create(recorder.getAudioSessionId());if(aec!=null)aec.setEnabled(false);
            ns=NoiseSuppressor.create(recorder.getAudioSessionId());if(ns!=null)ns.setEnabled(false);
            agc=AutomaticGainControl.create(recorder.getAudioSessionId());if(agc!=null)agc.setEnabled(false);
            player=new AudioTrack.Builder().setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION).setContentType(AudioAttributes.CONTENT_TYPE_SPEECH).build()).setAudioFormat(new AudioFormat.Builder().setSampleRate(48000).setEncoding(sampleFormat).setChannelMask(AudioFormat.CHANNEL_OUT_MONO).build()).setBufferSizeInBytes(Math.max(480*4*4,AudioTrack.getMinBufferSize(48000,AudioFormat.CHANNEL_OUT_MONO,sampleFormat))).setTransferMode(AudioTrack.MODE_STREAM).build();
            manager.setMode(AudioManager.MODE_IN_COMMUNICATION);
            if(Build.VERSION.SDK_INT>=31){for(AudioDeviceInfo device:manager.getAvailableCommunicationDevices())if(device.getType()==AudioDeviceInfo.TYPE_BUILTIN_SPEAKER){manager.setCommunicationDevice(device);break;}}
            else manager.setSpeakerphoneOn(true);
            recorder.startRecording();player.play();
            captureThread=new Thread(()->capture(created),"webrtc-native-capture");
            renderThread=new Thread(()->render(created),"webrtc-native-render");
            captureThread.start();renderThread.start();
            startCamera(created);
            failure="Native devices active";
        }catch(Exception error){failure="Device startup failed: "+error.getClass().getSimpleName();stopMedia();}
    }
    private void capture(long nativeHandle){
        Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO);
        ByteBuffer pcm=ByteBuffer.allocateDirect(480*4).order(ByteOrder.nativeOrder());
        AudioTimestamp timestamp=new AudioTimestamp();long readFrames=0;
        while(running.get()){
            pcm.clear();int bytes=recorder.read(pcm,pcm.capacity(),AudioRecord.READ_BLOCKING);
            if(bytes<=0)break;
            int count=bytes/4;long first=readFrames;readFrames+=count;
            if(recorder.getTimestamp(timestamp,AudioTimestamp.TIMEBASE_MONOTONIC)!=AudioRecord.SUCCESS)continue;
            long captured=timestamp.nanoTime+(first-timestamp.framePosition)*1_000_000_000L/48000;
            long age=System.nanoTime()-captured,now=NativeBridge.clockNs();
            if(age<0 || age>1_000_000_000L || age>now)continue;
            NativeBridge.capture(nativeHandle,pcm,count,now-age,now);
        }
    }
    private void render(long nativeHandle){
        Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO);
        ByteBuffer pcm=ByteBuffer.allocateDirect(480*4).order(ByteOrder.nativeOrder());
        AudioTimestamp timestamp=new AudioTimestamp();long written=0;
        while(running.get()){
            long now=NativeBridge.clockNs(),delay;
            if(player.getTimestamp(timestamp))delay=timestamp.nanoTime-System.nanoTime()+(written-timestamp.framePosition)*1_000_000_000L/48000;
            else delay=Math.max(0,written-Integer.toUnsignedLong(player.getPlaybackHeadPosition()))*1_000_000_000L/48000;
            NativeBridge.render(nativeHandle,pcm,480,now+Math.max(0,Math.min(1_000_000_000L,delay)));
            pcm.clear();int bytes=player.write(pcm,pcm.capacity(),AudioTrack.WRITE_BLOCKING);
            if(bytes<=0)break;written+=bytes/4;
        }
    }
    private void startCamera(long nativeHandle)throws CameraAccessException {
        CameraManager manager=getSystemService(CameraManager.class);
        String selected=null;
        for(String id:manager.getCameraIdList())if(Objects.equals(manager.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING),CameraCharacteristics.LENS_FACING_BACK)){selected=id;break;}
        if(selected==null)throw new IllegalStateException("Camera unavailable");
        CameraCharacteristics characteristics=manager.getCameraCharacteristics(selected);
        if(!Objects.equals(characteristics.get(CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE),CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE_REALTIME))throw new IllegalStateException("Camera clock unavailable");
        cameraImages=ImageReader.newInstance(640,480,ImageFormat.YUV_420_888,2);
        cameraImages.setOnImageAvailableListener(reader->{
            try(Image image=reader.acquireLatestImage()){
                if(image==null || !running.get())return;
                long age=SystemClock.elapsedRealtimeNanos()-image.getTimestamp(),now=NativeBridge.clockNs();
                if(age<0 || age>1_000_000_000L || age>now)return;
                long stamp=image.getTimestamp(),period=1_000_000_000L/videoFps;
                if(stamp+period/2<lastCamera)return;
                lastCamera=lastCamera!=0 && stamp-lastCamera<period?lastCamera+period:stamp+period;
                Image.Plane[] p=image.getPlanes();ByteBuffer y=p[0].getBuffer(),u=p[1].getBuffer(),v=p[2].getBuffer();
                NativeBridge.video(nativeHandle,1,image.getWidth(),image.getHeight(),y,y.position(),y.remaining(),p[0].getRowStride(),p[0].getPixelStride(),u,u.position(),u.remaining(),p[1].getRowStride(),p[1].getPixelStride(),v,v.position(),v.remaining(),p[2].getRowStride(),p[2].getPixelStride(),now-age);
            }
        },deviceHandler);
        manager.openCamera(selected,new CameraDevice.StateCallback(){
            public void onOpened(CameraDevice device){
                if(!running.get()){device.close();return;}camera=device;
                try{
                    Surface surface=cameraImages.getSurface();cameraRequest=device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD);cameraRequest.addTarget(surface);
                    cameraFpsRanges=characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES);
                    device.createCaptureSession(Collections.singletonList(surface),new CameraCaptureSession.StateCallback(){
                        public void onConfigured(CameraCaptureSession session){cameraSession=session;if(!running.get()){session.close();return;}applyCameraRate();}
                        public void onConfigureFailed(CameraCaptureSession session){failure="Camera configuration failed";}
                    },deviceHandler);
                }catch(CameraAccessException error){failure="Camera session failed";}
            }
            public void onDisconnected(CameraDevice device){device.close();failure="Camera disconnected";}
            public void onError(CameraDevice device,int error){device.close();failure="Camera error "+error;}
        },deviceHandler);
    }
    private void startProjection(int result,Intent consent){
        if(!running.get() || handle==0 || projection!=null)return;
        try{
            projection=getSystemService(MediaProjectionManager.class).getMediaProjection(result,consent);
            projectionCallback=new MediaProjection.Callback(){
                @Override public void onStop(){stopProjection();}
                @Override public void onCapturedContentResize(int width,int height){if(display!=null)resizeProjection(width,height);}
            };
            projection.registerCallback(projectionCallback,deviceHandler);
            android.util.DisplayMetrics metrics=getResources().getDisplayMetrics();
            createScreenImages(metrics.widthPixels,metrics.heightPixels);
            display=projection.createVirtualDisplay("LingXi WebRTC",screenImages.getWidth(),screenImages.getHeight(),metrics.densityDpi,DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,screenImages.getSurface(),null,deviceHandler);
            sharingScreen=true;
            bits|=8;synchronized(nativeLock){if(handle!=0)NativeBridge.controls(handle,bits);}
        }catch(Exception error){failure="Screen projection failed: "+error.getClass().getSimpleName();stopProjection();}
    }
    private void createScreenImages(int width,int height){
        float scale=Math.min(1f,1280f/Math.max(width,height));int w=Math.max(2,((int)(width*scale))&~1),h=Math.max(2,((int)(height*scale))&~1);
        screenImages=ImageReader.newInstance(w,h,PixelFormat.RGBA_8888,2);
        screenImages.setOnImageAvailableListener(reader->{
            try(Image image=reader.acquireLatestImage()){
                if(image==null || !running.get())return;
                screenCallbacks++;long now=NativeBridge.clockNs();long stamp=image.getTimestamp();
                long age=System.nanoTime()-stamp;screenAgeNs=age;
                if(age<0 || age>1_000_000_000L || age>now){screenInvalidTimestamp++;screenSubmitResult=-101;return;}
                // Schedule against capture time, not handler delivery jitter.
                // Select the sample nearest each capture deadline; a sample just
                // before that deadline must not halve a 30 Hz producer's rate.
                long period=1_000_000_000L/videoFps;
                if(stamp+period/2<lastScreen){screenRateLimited++;return;}
                lastScreen=lastScreen!=0 && stamp-lastScreen<period?lastScreen+period:stamp+period;
                Image.Plane plane=image.getPlanes()[0];ByteBuffer data=plane.getBuffer();
                screenSubmitResult=NativeBridge.rgba(handle,2,image.getWidth(),image.getHeight(),data,data.position(),data.remaining(),plane.getRowStride(),plane.getPixelStride(),now-age);if(screenSubmitResult==0)screenSubmitted++;
            }
        },deviceHandler);
    }
    private void resizeProjection(int width,int height){
        if(width<=0 || height<=0)return;
        float scale=Math.min(1f,1280f/Math.max(width,height));
        int w=Math.max(2,((int)(width*scale))&~1),h=Math.max(2,((int)(height*scale))&~1);
        // Updating the virtual display may emit the same content-size callback.
        // Preserve its surface when the scaled dimensions have not changed.
        if(screenImages!=null && screenImages.getWidth()==w && screenImages.getHeight()==h)return;
        ImageReader previous=screenImages;createScreenImages(width,height);
        display.resize(screenImages.getWidth(),screenImages.getHeight(),getResources().getDisplayMetrics().densityDpi);display.setSurface(screenImages.getSurface());if(previous!=null)previous.close();
    }
    private void stopProjection(){
        sharingScreen=false;
        bits&=~8;synchronized(nativeLock){if(handle!=0)NativeBridge.controls(handle,bits);}
        if(display!=null){display.release();display=null;}if(screenImages!=null){screenImages.close();screenImages=null;}
        MediaProjection old=projection;projection=null;
        MediaProjection.Callback callback=projectionCallback;projectionCallback=null;
        if(old!=null){if(callback!=null)old.unregisterCallback(callback);old.stop();}
    }
    public long[] stats(){
        long[] result;synchronized(nativeLock){result=handle==0?null:NativeBridge.stats(handle);}
        if(result!=null && result[0]<0){stopReason="Connection/media failed: "+result[1]+"; tap Start to reconnect";failure=stopReason;stopMedia();}
        return result;
    }
    public int[] takeVideo(ByteBuffer pixels){synchronized(nativeLock){return handle==0?null:NativeBridge.takeVideo(handle,pixels);}}
    public String status(){return failure;}
    public boolean isActive(){return running.get();}
    public int thermalStatus(){return thermalStatus;}
    public int videoTargetFps(){return videoFps;}
    public boolean isSharingScreen(){return sharingScreen;}
    public void testSpeaker(){synchronized(nativeLock){if(handle!=0)NativeBridge.controls(handle,bits|32);}}
    private void stopMedia(){
        if(!running.getAndSet(false))return;
        stopping.set(true);
        deviceHandler.post(()->{
            stopProjection();if(cameraSession!=null)cameraSession.close();if(camera!=null)camera.close();if(cameraImages!=null)cameraImages.close();
            try{if(recorder!=null)recorder.stop();}catch(IllegalStateException ignored){}
            try{if(player!=null)player.stop();}catch(IllegalStateException ignored){}
            new Thread(()->{
                try{if(captureThread!=null)captureThread.join();if(renderThread!=null)renderThread.join();}catch(InterruptedException error){Thread.currentThread().interrupt();return;}
                long old;synchronized(nativeLock){old=handle;handle=0;}if(old!=0)NativeBridge.close(old);
                if(recorder!=null)recorder.release();if(player!=null)player.release();if(aec!=null)aec.release();if(ns!=null)ns.release();if(agc!=null)agc.release();
                deviceHandler.post(()->{
                    recorder=null;player=null;aec=null;ns=null;agc=null;cameraSession=null;camera=null;cameraImages=null;cameraRequest=null;cameraFpsRanges=null;lastCamera=0;lastScreen=0;
                    AudioManager manager=getSystemService(AudioManager.class);if(Build.VERSION.SDK_INT>=31)manager.clearCommunicationDevice();manager.setMode(AudioManager.MODE_NORMAL);
                    stopping.set(false);failure=stopReason;recordLifecycle();
                    if(destroyed)deviceThread.quitSafely();
                });
            },"webrtc-device-cleanup").start();
        });
    }
    @Override public void onDestroy(){destroyed=true;if(Build.VERSION.SDK_INT>=29 && thermalListener!=null)powerManager.removeThermalStatusListener(thermalListener);stopMedia();if(!stopping.get())deviceThread.quitSafely();super.onDestroy();}
}
