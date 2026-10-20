#import "LXWebRTCCapture.h"
#import "NativeDevices.h"
#import <UIKit/UIKit.h>
#import <ReplayKit/ReplayKit.h>
#import <mach/mach_time.h>

// Blocks retain a lease while touching Rust. Detaching a device cannot free a
// handle underneath a callback; the last lease closes it on a background worker.
@interface LxProbeLease : NSObject
@property(nonatomic, assign) LxWebRtcProbe *handle;
@end
@implementation LxProbeLease
- (void)dealloc {
    LxWebRtcProbe *handle = _handle;
    if (handle) dispatch_async(dispatch_get_global_queue(QOS_CLASS_UTILITY, 0), ^{ lingxi_webrtc_probe_close(handle); });
}
@end

@interface LxNativeDevices ()
@property(atomic, strong) LxProbeLease *lease;
@property(nonatomic, strong) AVAudioEngine *engine;
@property(nonatomic, strong) AVAudioSourceNode *source;
@property(nonatomic, strong) AVCaptureSession *camera;
@property(nonatomic, strong) dispatch_queue_t controlQueue;
@property(nonatomic, strong) dispatch_queue_t cameraQueue;
@property(atomic, assign) BOOL screenActive;
@property(atomic, assign) uint64_t generation;
@property(nonatomic, assign) uint64_t lastScreen;
@property(atomic, copy) NSString *screenState;
@property(atomic, assign) uint64_t screenCallbacks;
@property(atomic, assign) uint64_t screenSubmitted;
@property(atomic, assign) int32_t screenSubmitResult;
@property(atomic, assign) double screenSampleAge;
@end
@implementation LxNativeDevices
- (instancetype)init {
    if ((self = [super init])) {
        _controlQueue = dispatch_queue_create("lingxi.webrtc.devices", DISPATCH_QUEUE_SERIAL);
        _cameraQueue = dispatch_queue_create("lingxi.webrtc.camera", DISPATCH_QUEUE_SERIAL);
        NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
        [center addObserver:self selector:@selector(suspended:) name:UIApplicationDidEnterBackgroundNotification object:nil];
        [center addObserver:self selector:@selector(suspended:) name:AVAudioSessionInterruptionNotification object:nil];
        [center addObserver:self selector:@selector(suspended:) name:AVAudioSessionRouteChangeNotification object:nil];
    }
    return self;
}
- (void)dealloc { [NSNotificationCenter.defaultCenter removeObserver:self]; }
- (void)publish:(NSString *)message {
    dispatch_async(dispatch_get_main_queue(), ^{ if (self.status) self.status(message); });
}
- (void)suspended:(NSNotification *)notification {
    if (!self.lease) return;
    [self stop];
    [self publish:@"Device suspended, interrupted or route changed. Media stopped; restart to use the current devices."];
}
- (void)startWithEndpoint:(NSString *)endpoint token:(NSString *)token {
    [self stop];
    uint64_t generation = self.generation;
    [AVAudioApplication requestRecordPermissionWithCompletionHandler:^(BOOL audioAllowed) {
        if (!audioAllowed) { [self publish:@"Microphone permission denied."]; return; }
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL videoAllowed) {
            if (!videoAllowed) { [self publish:@"Camera permission denied."]; return; }
            dispatch_async(self.controlQueue, ^{
                if (generation != self.generation) return;
                NSError *error = nil;
                AVAudioSession *session = AVAudioSession.sharedInstance;
                if (![session setCategory:AVAudioSessionCategoryPlayAndRecord mode:AVAudioSessionModeDefault options:AVAudioSessionCategoryOptionDefaultToSpeaker|AVAudioSessionCategoryOptionAllowBluetooth error:&error] ||
                    ![session setPreferredSampleRate:48000 error:&error] ||
                    ![session setPreferredIOBufferDuration:0.01 error:&error] ||
                    ![session setActive:YES error:&error]) { [self publish:@"Audio session could not start."]; return; }
                AVAudioEngine *engine = [AVAudioEngine new];
                AVAudioInputNode *input = engine.inputNode;
                // Do not enable Apple's voice processor: Sonora owns AEC/NS.
                AVAudioFormat *inputFormat = [input outputFormatForBus:0];
                AVAudioFormat *outputFormat = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:session.sampleRate channels:1];
                if (inputFormat.commonFormat != AVAudioPCMFormatFloat32 || inputFormat.sampleRate < 8000 || !outputFormat) { [self publish:@"Unsupported native audio format."]; return; }
                NSString *ice=[NSBundle.mainBundle objectForInfoDictionaryKey:@"LingxiWebRTCIceConfig"];
                NSData *iceData=ice.length?[ice dataUsingEncoding:NSUTF8StringEncoding]:nil;
                LxWebRtcProbe *handle = lingxi_webrtc_probe_open(endpoint.UTF8String, token.UTF8String, (uint32_t)inputFormat.sampleRate, (uint32_t)session.sampleRate, iceData.bytes, iceData.length);
                if (!handle) { [self publish:@"Rust media initialization failed."]; return; }
                LxProbeLease *lease = [LxProbeLease new]; lease.handle = handle;
                self.lease = lease;
                lingxi_webrtc_probe_controls(handle, 1|2|4|16);
                [input installTapOnBus:0 bufferSize:1024 format:inputFormat block:^(AVAudioPCMBuffer *buffer, AVAudioTime *when) {
                    AVAudioFrameCount count = buffer.frameLength;
                    if (!count || count > 8192 || !when.hostTimeValid || !buffer.floatChannelData) return;
                    uint64_t now = lingxi_webrtc_clock_ns();
                    double age = [AVAudioTime secondsForHostTime:mach_absolute_time()] - [AVAudioTime secondsForHostTime:when.hostTime];
                    if (age < -0.001 || age > 1.) return;
                    uint64_t delta = (uint64_t)(MAX(0.,age)*1e9);
                    if (delta > now) return;
                    float mono[8192];
                    for (AVAudioFrameCount n=0;n<count;n++) {
                        float sum=0.;
                        for (AVAudioChannelCount channel=0;channel<buffer.format.channelCount;channel++) sum+=buffer.floatChannelData[channel][n];
                        mono[n]=sum/MAX(1,buffer.format.channelCount);
                    }
                    lingxi_webrtc_probe_capture(lease.handle, mono, count, now-delta, now);
                }];
                AVAudioSourceNode *source = [[AVAudioSourceNode alloc] initWithFormat:outputFormat renderBlock:^OSStatus(BOOL *isSilence,const AudioTimeStamp *time,AVAudioFrameCount count,AudioBufferList *data) {
                    for (UInt32 b=0;b<data->mNumberBuffers;b++) memset(data->mBuffers[b].mData,0,data->mBuffers[b].mDataByteSize);
                    if (data->mNumberBuffers!=1 || count>8192) return noErr;
                    uint64_t now=lingxi_webrtc_clock_ns();
                    double delay=(time->mFlags & kAudioTimeStampHostTimeValid) ? [AVAudioTime secondsForHostTime:time->mHostTime]-[AVAudioTime secondsForHostTime:mach_absolute_time()] : session.outputLatency;
                    uint64_t playback=now+(uint64_t)(MAX(0.,MIN(delay,1.))*1e9);
                    lingxi_webrtc_probe_render(lease.handle,data->mBuffers[0].mData,count,playback);
                    *isSilence=NO;
                    return noErr;
                }];
                [engine attachNode:source];
                [engine connect:source to:engine.mainMixerNode format:outputFormat];
                self.engine=engine;self.source=source;
                [engine prepare];
                if (![engine startAndReturnError:&error]) { [self stop]; [self publish:@"Native audio device start failed."]; return; }
                AVCaptureDevice *camera=[AVCaptureDevice defaultDeviceWithMediaType:AVMediaTypeVideo];
                AVCaptureDeviceInput *cameraInput=[AVCaptureDeviceInput deviceInputWithDevice:camera error:&error];
                AVCaptureSession *capture=[AVCaptureSession new];
                // AVAudioEngine owns the shared session; the video-only session
                // must not reconfigure it while ReplayKit starts app capture.
                capture.automaticallyConfiguresApplicationAudioSession=NO;
                capture.sessionPreset=AVCaptureSessionPreset640x480;
                AVCaptureVideoDataOutput *output=[AVCaptureVideoDataOutput new];
                output.videoSettings=@{(id)kCVPixelBufferPixelFormatTypeKey:@(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)};
                output.alwaysDiscardsLateVideoFrames=YES;
                [output setSampleBufferDelegate:self queue:self.cameraQueue];
                if (!cameraInput || ![capture canAddInput:cameraInput] || ![capture canAddOutput:output]) { [self stop]; [self publish:@"Native camera unavailable."]; return; }
                [capture addInput:cameraInput];[capture addOutput:output];
                if ([camera lockForConfiguration:nil]) {
                    for (AVFrameRateRange *range in camera.activeFormat.videoSupportedFrameRateRanges) if (range.minFrameRate<=30 && range.maxFrameRate>=30) {camera.activeVideoMinFrameDuration=CMTimeMake(1,30);camera.activeVideoMaxFrameDuration=CMTimeMake(1,30);break;}
                    [camera unlockForConfiguration];
                }
                self.camera=capture;[capture startRunning];
                [self publish:@"Native microphone and camera ready. Speaker test plays only when requested."];
            });
        }];
    }];
}
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sample fromConnection:(AVCaptureConnection *)connection {
    LxProbeLease *lease=self.lease;
    CVPixelBufferRef pixels=CMSampleBufferGetImageBuffer(sample);
    if (lease && pixels) LXWebRTCSubmitVideo(lease.handle,pixels,CMSampleBufferGetPresentationTimeStamp(sample),1);
}
- (void)testSpeaker {
    LxProbeLease *lease=self.lease;
    if(lease)lingxi_webrtc_probe_controls(lease.handle,1|2|4|16|(self.screenActive?8:0)|32);
}
- (NSDictionary *)screenDiagnostics {
    return @{@"state":self.screenState?:@"idle",@"callbacks":@(self.screenCallbacks),@"submitted":@(self.screenSubmitted),@"submitResult":@(self.screenSubmitResult),@"sampleAge":@(isfinite(self.screenSampleAge)?self.screenSampleAge:0),@"available":@(RPScreenRecorder.sharedRecorder.available),@"recording":@(RPScreenRecorder.sharedRecorder.recording)};
}
- (void)startAppScreen {
    LxProbeLease *lease=self.lease;
    if (!lease || self.screenActive) return;
    self.screenActive=YES;
    self.screenState=@"starting";self.screenCallbacks=0;self.screenSubmitted=0;self.screenSubmitResult=0;self.lastScreen=0;
    lingxi_webrtc_probe_controls(lease.handle,1|2|4|8|16);
    __weak LxNativeDevices *weakSelf=self;
    RPScreenRecorder *recorder=RPScreenRecorder.sharedRecorder;
    recorder.microphoneEnabled=NO;
    recorder.cameraEnabled=NO;
    [recorder startCaptureWithHandler:^(CMSampleBufferRef sample,RPSampleBufferType type,NSError *error) {
        LxNativeDevices *owner=weakSelf;
        if (!owner || !owner.screenActive || error || type!=RPSampleBufferTypeVideo) return;
        owner.screenCallbacks+=1;
        owner.screenSampleAge=CMTimeGetSeconds(CMTimeSubtract(CMClockGetTime(CMClockGetHostTimeClock()),CMSampleBufferGetPresentationTimeStamp(sample)));
        uint64_t now=lingxi_webrtc_clock_ns();
        if (now<owner.lastScreen) return;
        const uint64_t period=1000000000/30;
        owner.lastScreen=owner.lastScreen && now-owner.lastScreen<period ? owner.lastScreen+period : now+period;
        CVPixelBufferRef pixels=CMSampleBufferGetImageBuffer(sample);
        if (pixels) {owner.screenSubmitResult=LXWebRTCSubmitVideo(owner.lease.handle,pixels,CMSampleBufferGetPresentationTimeStamp(sample),2);if(owner.screenSubmitResult==0)owner.screenSubmitted+=1;}
    } completionHandler:^(NSError *error) {
        if (error) {self.screenState=[NSString stringWithFormat:@"failed: %ld %@",(long)error.code,error.description];self.screenActive=NO;lingxi_webrtc_probe_controls(lease.handle,1|2|4|16);[self publish:self.screenState];}
        else {self.screenState=@"active";[self publish:@"ReplayKit app-screen capture started. Cross-app broadcast is a separate extension capability."];}
    }];
}
- (void)stop {
    self.generation+=1;
    self.screenActive=NO;
    if (RPScreenRecorder.sharedRecorder.isRecording) [RPScreenRecorder.sharedRecorder stopCaptureWithHandler:^(NSError *error) {}];
    dispatch_async(self.controlQueue, ^{
        LxProbeLease *lease=self.lease;
        self.lease=nil;
        if (lease) lingxi_webrtc_probe_controls(lease.handle,0);
        [self.engine stop];
        if (self.engine) [self.engine.inputNode removeTapOnBus:0];
        if (self.source) [self.engine detachNode:self.source];
        self.source=nil;self.engine=nil;
        [self.camera stopRunning];self.camera=nil;
        [AVAudioSession.sharedInstance setActive:NO withOptions:AVAudioSessionSetActiveOptionNotifyOthersOnDeactivation error:nil];
        NSDictionary *receipt=@{@"recordedAt":@(NSDate.date.timeIntervalSince1970),@"phase":@"devices-stopped",@"running":@NO,@"sharingScreen":@NO};
        NSURL *documents=[NSFileManager.defaultManager URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
        [[NSJSONSerialization dataWithJSONObject:receipt options:0 error:nil] writeToURL:[documents URLByAppendingPathComponent:@"webrtc-lifecycle-receipt.json"] atomically:YES];
    });
}
- (BOOL)stats:(LxWebRtcProbeStats *)stats {LxProbeLease *lease=self.lease;return lease && lingxi_webrtc_probe_stats(lease.handle,stats)==0;}
- (BOOL)takeVideo:(LxWebRtcVideoMeta *)meta pixels:(uint8_t *)pixels capacity:(size_t)capacity {LxProbeLease *lease=self.lease;return lease && lingxi_webrtc_probe_take_video(lease.handle,meta,pixels,capacity)==1;}
@end
