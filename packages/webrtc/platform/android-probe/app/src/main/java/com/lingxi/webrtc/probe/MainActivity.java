package com.lingxi.webrtc.probe;

import android.Manifest;
import android.app.*;
import android.content.*;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.media.projection.MediaProjectionManager;
import android.os.*;
import android.view.*;
import android.widget.*;
import java.nio.*;
import java.io.*;
import org.json.JSONObject;

public final class MainActivity extends Activity {
    private TextView report;
    private EditText endpoint;
    private ImageView preview;
    private Bitmap bitmap;
    private View motion;
    private long benchmarkUntil;
    private boolean resumed;
    private boolean previewHidden;
    private MediaService service;
    private boolean bound;
    private final Handler ui=new Handler(Looper.getMainLooper());
    private final ByteBuffer pixels=ByteBuffer.allocateDirect(4*1024*1024).order(ByteOrder.nativeOrder());
    private long lastReceipt;
    private long lastCameraFrames,lastScreenFrames;
    private final ServiceConnection connection=new ServiceConnection(){
        public void onServiceConnected(ComponentName name,IBinder binder){service=((MediaService.LocalBinder)binder).service();}
        public void onServiceDisconnected(ComponentName name){service=null;}
    };
    @Override public void onCreate(Bundle saved){
        super.onCreate(saved);
        LinearLayout content=new LinearLayout(this);content.setOrientation(LinearLayout.VERTICAL);content.setPadding(24,48,24,24);
        report=new TextView(this);report.setText("LingXi WebRTC\nVerifying shared Rust codecs…");report.setTextSize(16);report.setContentDescription("mediaReport");
        content.addView(report,new LinearLayout.LayoutParams(-1,0,1));
        motion=new View(this){
            final Paint paint=new Paint();
            @Override protected void onDraw(Canvas canvas){
                super.onDraw(canvas);if(!benchmarking())return;paint.setColor(0xffff9500);
                float x=(SystemClock.uptimeMillis()%2000)/2000f*Math.max(1,getWidth()-32);
                canvas.drawRect(x,0,x+32,getHeight(),paint);postInvalidateOnAnimation();
            }
        };
        content.addView(motion,new LinearLayout.LayoutParams(-1,12));
        preview=new ImageView(this);preview.setScaleType(ImageView.ScaleType.FIT_CENTER);content.addView(preview,new LinearLayout.LayoutParams(-1,400));
        endpoint=new EditText(this);endpoint.setText("192.168.0.2:18080");endpoint.setSingleLine(true);content.addView(endpoint);
        addButton(content,"Start audio and camera",()->requestDevices());
        addButton(content,"Test speaker — quiet 1 second",()->{if(service!=null)service.testSpeaker();});
        addButton(content,"Share screen",()->{if(service!=null)startActivityForResult(getSystemService(MediaProjectionManager.class).createScreenCaptureIntent(),2);});
        addButton(content,"Run motion test — 30 seconds",()->{
            if(service==null || !service.isActive()){
                Toast.makeText(this,"Start media before running the motion test",Toast.LENGTH_SHORT).show();
            }else if(service.thermalStatus()>=PowerManager.THERMAL_STATUS_MODERATE){
                Toast.makeText(this,"Device is warm; motion test is paused until it cools",Toast.LENGTH_LONG).show();
            }else{
                benchmarkUntil=SystemClock.elapsedRealtime()+30_000;motion.invalidate();
            }
        });
        addButton(content,"Stop media",()->stopMedia());setContentView(content);
        new Thread(()->{
            int packets=NativeBridge.verifyCodecs();
            try{writeReceipt("webrtc-codec-receipt.json",new JSONObject().put("fixture","shared-codecs-v1").put("passed",packets>0).put("opus20msPackets",packets).put("vp8Frames",packets>0?5:0).put("deviceCaptureVerified",false).put("remoteVerified",false));}catch(Exception ignored){}
            ui.post(()->{report.setText("LingXi WebRTC\n"+(packets>0?"PASS":"FAILED")+" — shared Sonora/Rubato/Opus: "+packets+" × 20ms; VP8: "+(packets>0?5:0)+" frames");if(packets>0 && getIntent().getBooleanExtra("autoStart",false))requestDevices();});
        },"webrtc-codec-acceptance").start();
    }
    private boolean benchmarking(){
        if(!resumed || service==null || !service.isActive() || service.thermalStatus()>=PowerManager.THERMAL_STATUS_MODERATE)benchmarkUntil=0;
        return SystemClock.elapsedRealtime()<benchmarkUntil;
    }
    @Override protected void onResume(){super.onResume();resumed=true;ui.removeCallbacks(poll);ui.post(poll);}
    @Override protected void onPause(){resumed=false;benchmarkUntil=0;ui.removeCallbacks(poll);motion.invalidate();super.onPause();}
    @Override protected void onStart(){
        super.onStart();
        // System projection selection may recreate this Activity while the
        // foreground media service continues using the existing consent.
        if(!bound)bound=bindService(new Intent(this,MediaService.class),connection,0);
    }
    private void addButton(LinearLayout parent,String title,Runnable action){Button button=new Button(this);button.setText(title);button.setOnClickListener(view->action.run());parent.addView(button);}
    private void requestDevices(){
        if(checkSelfPermission(Manifest.permission.RECORD_AUDIO)!=PackageManager.PERMISSION_GRANTED || checkSelfPermission(Manifest.permission.CAMERA)!=PackageManager.PERMISSION_GRANTED){requestPermissions(new String[]{Manifest.permission.RECORD_AUDIO,Manifest.permission.CAMERA},1);return;}
        if(Build.VERSION.SDK_INT>=33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)!=PackageManager.PERMISSION_GRANTED)requestPermissions(new String[]{Manifest.permission.POST_NOTIFICATIONS},3);
        startMedia();
    }
    @Override public void onRequestPermissionsResult(int request,String[] permissions,int[] results){super.onRequestPermissionsResult(request,permissions,results);if(request==1 && results.length==2 && results[0]==0 && results[1]==0)requestDevices();}
    private void startMedia(){
        Intent intent=new Intent(this,MediaService.class).setAction(MediaService.START).putExtra("endpoint",endpoint.getText().toString()).putExtra("iceConfig",getIntent().getStringExtra("iceConfig"));
        startForegroundService(intent);if(!bound)bound=bindService(new Intent(this,MediaService.class),connection,BIND_AUTO_CREATE);
    }
    private void stopMedia(){
        benchmarkUntil=0;motion.invalidate();
        if(bound){unbindService(connection);bound=false;}service=null;
        startService(new Intent(this,MediaService.class).setAction(MediaService.STOP));
        report.setText("Media stopped");
    }
    @Override protected void onActivityResult(int request,int result,Intent data){
        super.onActivityResult(request,result,data);
        if(request==2 && result==RESULT_OK && data!=null && bound)startForegroundService(new Intent(this,MediaService.class).setAction(MediaService.PROJECT).putExtra("result",result).putExtra("consent",data));
    }
    private final Runnable poll=new Runnable(){public void run(){
        MediaService current=service;
        if(current!=null){
            long[] stats=current.stats();
            if(stats!=null){
                long now=SystemClock.elapsedRealtime();
                if(now-lastReceipt>=500){
                    double seconds=(now-lastReceipt)/1000.;
                    double cameraFPS=stats[7]>=lastCameraFrames?(stats[7]-lastCameraFrames)/seconds:0;
                    double screenFPS=stats[8]>=lastScreenFrames?(stats[8]-lastScreenFrames)/seconds:0;
                    lastReceipt=now;lastCameraFrames=stats[7];lastScreenFrames=stats[8];
                    report.setText("LingXi WebRTC — device fixture\n"+current.status()+"\nThermal: "+current.thermalStatus()+"  Video target: "+current.videoTargetFps()+" fps\nState: "+stats[0]+"  Failure: "+stats[1]+"\nCaptured: "+stats[2]+"  Rendered: "+stats[3]+"\nMic peak: "+stats[5]+"  Speaker peak: "+stats[6]+"\nCamera: "+stats[7]+String.format(java.util.Locale.ROOT," (%.1f fps)  Screen: %d (%.1f fps)",cameraFPS,stats[8],screenFPS));
                    try{writeReceipt("webrtc-media-receipt.json",new JSONObject().put("fixture","native-media-v1").put("recordedAt",System.currentTimeMillis()/1000.).put("connected",stats[0]==1).put("failure",stats[1]).put("capturedSamples",stats[2]).put("renderedSamples",stats[3]).put("microphonePeak",stats[5]).put("speakerPeak",stats[6]).put("decodedCameraFrames",stats[7]).put("cameraFPS",cameraFPS).put("decodedScreenFrames",stats[8]).put("screenFPS",screenFPS).put("status",current.status()).put("thermalStatus",current.thermalStatus()).put("videoTargetFPS",current.videoTargetFps()).put("benchmarkActive",benchmarking()).put("sharingScreen",current.isSharingScreen()).put("screenCallbacks",current.screenCallbacks).put("screenSubmitted",current.screenSubmitted).put("screenAgeNs",current.screenAgeNs).put("screenRateLimited",current.screenRateLimited).put("screenInvalidTimestamp",current.screenInvalidTimestamp).put("screenSubmitResult",current.screenSubmitResult).put("localRelayCandidates",stats[9]).put("remoteRelayCandidates",stats[10]).put("relayRequired",stats[11]==1).put("remoteVerified",false));}catch(Exception ignored){}
                }
                // Returning our own shared screen into the captured UI creates a
                // perpetual redraw/encode feedback loop. Only the bounded motion
                // benchmark intentionally renders the returned screen here.
                boolean hide=current.isSharingScreen() && !benchmarking();
                if(hide!=previewHidden){previewHidden=hide;preview.setVisibility(hide?View.INVISIBLE:View.VISIBLE);}
                int[] meta=null;
                if(!hide)for(int n=0;n<4;n++){int[] next=current.takeVideo(pixels);if(next==null)break;if(next[3]==(current.isSharingScreen()?2:1)){meta=next;break;}}
                if(meta!=null){if(bitmap==null || bitmap.getWidth()!=meta[0] || bitmap.getHeight()!=meta[1]){bitmap=Bitmap.createBitmap(meta[0],meta[1],Bitmap.Config.ARGB_8888);preview.setImageBitmap(bitmap);}NativeBridge.renderBitmap(pixels,meta,bitmap);preview.invalidate();}
            }else{report.setText("LingXi WebRTC — device fixture\n"+current.status());}
        }
        if(resumed)ui.postDelayed(this,current!=null && current.isActive()?33:250);
    }};
    private void writeReceipt(String name,JSONObject receipt)throws IOException {try(FileOutputStream output=openFileOutput(name,MODE_PRIVATE)){output.write(receipt.toString().getBytes(java.nio.charset.StandardCharsets.UTF_8));}}
    @Override protected void onDestroy(){ui.removeCallbacks(poll);if(bound){unbindService(connection);bound=false;}super.onDestroy();}
}
