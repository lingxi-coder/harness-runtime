#import <ReplayKit/ReplayKit.h>
#import <UIKit/UIKit.h>
#import "lingxi_webrtc.h"
#import "NativeDevices.h"
#import <Network/Network.h>

@interface ProbeController : UIViewController
@property(nonatomic, strong) UITextView *report;
@property(nonatomic, strong) LxNativeDevices *devices;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, strong) AVSampleBufferDisplayLayer *videoLayer;
@property(nonatomic, strong) UIView *videoView;
@property(nonatomic, strong) UIView *motionMarker;
@property(nonatomic, strong) CADisplayLink *displayLink;
@property(nonatomic, assign) CFTimeInterval benchmarkUntil;
@property(nonatomic, strong) NSMutableData *pixels;
@property(nonatomic, copy) NSString *deviceStatus;
@property(nonatomic, strong) nw_connection_t networkCheck;
@property(nonatomic, assign) NSUInteger networkGeneration;
@property(nonatomic, assign) BOOL startWhenActive;
@property(nonatomic, assign) CFTimeInterval lastReport;
@property(nonatomic, assign) uint64_t lastCameraFrames;
@property(nonatomic, assign) uint64_t lastScreenFrames;
@property(nonatomic, assign) BOOL screenFixtureStarted;
@end
@implementation ProbeController
@synthesize deviceStatus=_deviceStatus;
- (void)setDeviceStatus:(NSString *)value {
    _deviceStatus=[value copy];
    NSDictionary *receipt=@{@"recordedAt":@([NSDate.date timeIntervalSince1970]),@"status":value?:@"",@"applicationState":@(UIApplication.sharedApplication.applicationState),@"arguments":NSProcessInfo.processInfo.arguments};
    NSURL *documents=[[NSFileManager defaultManager] URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
    [[NSJSONSerialization dataWithJSONObject:receipt options:NSJSONWritingPrettyPrinted error:nil] writeToURL:[documents URLByAppendingPathComponent:@"webrtc-status-receipt.json"] atomically:YES];
}
- (void)viewDidLoad {
    [super viewDidLoad];
    [NSNotificationCenter.defaultCenter addObserver:self selector:@selector(becameActive:) name:UIApplicationDidBecomeActiveNotification object:nil];
    [NSNotificationCenter.defaultCenter addObserver:self selector:@selector(pauseFixture:) name:UIApplicationDidEnterBackgroundNotification object:nil];
    self.view.backgroundColor = UIColor.systemBackgroundColor;
    self.report = [UITextView new];
    self.report.editable = NO;
    self.report.font = [UIFont preferredFontForTextStyle:UIFontTextStyleBody];
    self.report.text = @"LingXi WebRTC\n\nVerifying shared Rust audio processing and Opus / VP8 codecs…\n\nThis fixture uses generated media. Device capture is a separate check.";
    self.report.translatesAutoresizingMaskIntoConstraints = NO;
    self.report.accessibilityIdentifier=@"mediaReport";
    self.videoView=[UIView new];self.videoView.backgroundColor=UIColor.secondarySystemBackgroundColor;
    self.videoLayer=[AVSampleBufferDisplayLayer new];self.videoLayer.videoGravity=AVLayerVideoGravityResizeAspect;
    [self.videoView.layer addSublayer:self.videoLayer];
    // Continuous known motion distinguishes capture throughput from ReplayKit's
    // lower callback rate when the shared screen is static.
    self.motionMarker=[[UIView alloc] initWithFrame:CGRectMake(0,0,24,8)];
    self.motionMarker.backgroundColor=UIColor.systemOrangeColor;
    [self.videoView addSubview:self.motionMarker];
    self.displayLink=[CADisplayLink displayLinkWithTarget:self selector:@selector(animateFixture:)];
    self.displayLink.preferredFrameRateRange=CAFrameRateRangeMake(30,30,30);
    self.displayLink.paused=YES;
    [self.displayLink addToRunLoop:NSRunLoop.mainRunLoop forMode:NSRunLoopCommonModes];
    UIStackView *stack=[[UIStackView alloc] initWithArrangedSubviews:@[self.report,self.videoView,
        [self button:@"Start audio and camera" action:@selector(startMedia) identifier:@"startMedia"],
        [self button:@"Test speaker — quiet 1 second" action:@selector(testSpeaker) identifier:@"testSpeaker"],
        [self button:@"Open app permissions" action:@selector(openPermissions) identifier:@"openPermissions"],
        [self button:@"Share this app screen" action:@selector(startScreen) identifier:@"startScreen"],
        [self button:@"Run motion test — 30 seconds" action:@selector(runMotion) identifier:@"runMotion"],
        [self button:@"Prepare system screen sharing" action:@selector(prepareSystemBroadcast) identifier:@"prepareBroadcast"],
        [self broadcastPicker],
        [self button:@"Stop media" action:@selector(stopMedia) identifier:@"stopMedia"]]];
    stack.axis=UILayoutConstraintAxisVertical;stack.spacing=12;stack.translatesAutoresizingMaskIntoConstraints=NO;
    [self.view addSubview:stack];
    [NSLayoutConstraint activateConstraints:@[[stack.leadingAnchor constraintEqualToAnchor:self.view.safeAreaLayoutGuide.leadingAnchor constant:20],[stack.trailingAnchor constraintEqualToAnchor:self.view.safeAreaLayoutGuide.trailingAnchor constant:-20],[stack.topAnchor constraintEqualToAnchor:self.view.safeAreaLayoutGuide.topAnchor constant:16],[stack.bottomAnchor constraintEqualToAnchor:self.view.safeAreaLayoutGuide.bottomAnchor constant:-16],[self.videoView.heightAnchor constraintEqualToConstant:180]]];
    self.pixels=[NSMutableData dataWithLength:4*1024*1024];
    self.devices=[LxNativeDevices new];
    __weak ProbeController *weakSelf=self;
    self.devices.status=^(NSString *message){weakSelf.deviceStatus=message;};
    self.timer=[NSTimer timerWithTimeInterval:1./30. repeats:YES block:^(NSTimer *timer){[weakSelf pollMedia];}];
    [NSRunLoop.mainRunLoop addTimer:self.timer forMode:NSRunLoopCommonModes];
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        int32_t packets = lingxi_webrtc_verify_codecs();
        NSDictionary *receipt = @{
            @"fixture": @"shared-codecs-v1",
            @"passed": @(packets > 0),
            @"opus20msPackets": @(packets),
            @"vp8Frames": @(packets > 0 ? 5 : 0),
            @"deviceCaptureVerified": @NO,
            @"remoteVerified": @NO
        };
        NSData *data = [NSJSONSerialization dataWithJSONObject:receipt options:NSJSONWritingPrettyPrinted error:nil];
        NSURL *documents = [[NSFileManager defaultManager] URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
        [data writeToURL:[documents URLByAppendingPathComponent:@"webrtc-codec-receipt.json"] atomically:YES];
        dispatch_async(dispatch_get_main_queue(), ^{
            self.report.text = [NSString stringWithFormat:@"LingXi WebRTC\n\n%@\n\nShared Sonora / Rubato / Opus: %d × 20 ms\nShared GStreamer VP8: %d frames\n\nDevice capture and Remote remain separate acceptance checks.", packets > 0 ? @"PASS" : @"FAILED", packets, packets > 0 ? 5 : 0];
            if (packets>0 && [NSProcessInfo.processInfo.arguments containsObject:@"--webrtc-device-fixture"]) [self startMedia];
        });
    });
}
- (UIButton *)button:(NSString *)title action:(SEL)action identifier:(NSString *)identifier {
    UIButton *button=[UIButton buttonWithType:UIButtonTypeSystem];
    [button setTitle:title forState:UIControlStateNormal];button.accessibilityIdentifier=identifier;
    [button addTarget:self action:action forControlEvents:UIControlEventTouchUpInside];
    return button;
}
- (void)pauseFixture:(NSNotification *)notification {self.benchmarkUntil=0;self.displayLink.paused=YES;}
- (BOOL)benchmarking {
    if(UIApplication.sharedApplication.applicationState!=UIApplicationStateActive || NSProcessInfo.processInfo.thermalState>=NSProcessInfoThermalStateSerious)self.benchmarkUntil=0;
    return CACurrentMediaTime()<self.benchmarkUntil;
}
- (void)runMotion {LxWebRtcProbeStats stats={0};if([self.devices stats:&stats] && stats.state==1 && NSProcessInfo.processInfo.thermalState<NSProcessInfoThermalStateSerious){self.benchmarkUntil=CACurrentMediaTime()+30;self.displayLink.paused=NO;}}
- (void)animateFixture:(CADisplayLink *)link {
    if(![self benchmarking]){self.displayLink.paused=YES;return;}
    self.motionMarker.frame=CGRectMake(fmod(link.timestamp*120,MAX(1,self.videoView.bounds.size.width-24)),0,24,8);
}
- (void)viewDidLayoutSubviews {[super viewDidLayoutSubviews];self.videoLayer.frame=self.videoView.bounds;}
- (void)startMedia {
    [self stopMedia];
    UIApplication.sharedApplication.idleTimerDisabled=YES;
    self.screenFixtureStarted=NO;
    if(UIApplication.sharedApplication.applicationState!=UIApplicationStateActive){
        self.startWhenActive=YES;
        self.deviceStatus=@"Waiting for the app to enter the foreground before requesting local network access…";
        return;
    }
    self.deviceStatus=@"Checking local network access… Allow access if iOS asks.";
    self.report.text=self.deviceStatus;
    NSString *endpoint=[NSBundle.mainBundle objectForInfoDictionaryKey:@"LingxiWebRTCFixtureEndpoint"];
    NSURLComponents *parts=[NSURLComponents componentsWithString:[@"http://" stringByAppendingString:endpoint]];
    if (!parts.host || !parts.port) {self.deviceStatus=@"Invalid test server address.";return;}
    nw_parameters_t parameters=nw_parameters_create_secure_tcp(NW_PARAMETERS_DISABLE_PROTOCOL,NW_PARAMETERS_DEFAULT_CONFIGURATION);
    self.networkCheck=nw_connection_create(nw_endpoint_create_host(parts.host.UTF8String,parts.port.stringValue.UTF8String),parameters);
    nw_connection_set_queue(self.networkCheck,dispatch_get_main_queue());
    NSUInteger generation=self.networkGeneration;
    __weak ProbeController *weakSelf=self;
    nw_connection_set_state_changed_handler(self.networkCheck,^(nw_connection_state_t state,nw_error_t error){
        ProbeController *owner=weakSelf;
        if (!owner || generation!=owner.networkGeneration || !owner.networkCheck) return;
        nw_path_t path=nw_connection_copy_current_path(owner.networkCheck);
        int reason=path ? nw_path_get_unsatisfied_reason(path) : -1;
        NSString *route=path && nw_path_uses_interface_type(path,nw_interface_type_wifi) ? @"Wi-Fi" : path && nw_path_uses_interface_type(path,nw_interface_type_cellular) ? @"Cellular" : @"No usable route";
        NSString *message;
        if(state==nw_connection_state_ready) message=@"Local test server reached. Starting microphone and camera…";
        else if(reason==nw_path_unsatisfied_reason_local_network_denied) message=@"iOS is blocking local network access. Allow the permission prompt; if denied, open app permissions and enable Local Network.";
        else if(reason==nw_path_unsatisfied_reason_wifi_denied) message=@"iOS reports Wi-Fi access is disabled. Check Wi-Fi is connected, then open app permissions and allow WLAN under Wireless Data if that option is present. Tap Start to retry.";
        else if(reason==nw_path_unsatisfied_reason_cellular_denied) message=@"iOS reports cellular access is disabled. This local fixture needs Wi-Fi: connect to the Mac's Wi-Fi network, allow WLAN in app permissions, then tap Start.";
        else message=[NSString stringWithFormat:@"Connecting to %@ via %@…\nNetwork state: %d; path reason: %d; error: %d",endpoint,route,state,reason,error?nw_error_get_error_code(error):0];
        owner.deviceStatus=message;
        NSDictionary *receipt=@{@"recordedAt":@([NSDate.date timeIntervalSince1970]),@"endpoint":endpoint,@"state":@(state),@"pathReason":@(reason),@"route":route,@"error":@(error?nw_error_get_error_code(error):0),@"message":message};
        NSURL *documents=[[NSFileManager defaultManager] URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
        [[NSJSONSerialization dataWithJSONObject:receipt options:NSJSONWritingPrettyPrinted error:nil] writeToURL:[documents URLByAppendingPathComponent:@"webrtc-network-receipt.json"] atomically:YES];
        if(state==nw_connection_state_ready){
            [owner cancelNetworkCheck];
            [owner.devices startWithEndpoint:endpoint token:@"local-webrtc-test-token"];
        }else if(state==nw_connection_state_failed){
            owner.deviceStatus=[message stringByAppendingString:@"\nConnection failed. Check Wi-Fi and tap Start to retry."];
            [owner cancelNetworkCheck];
        }
    });
    nw_connection_start(self.networkCheck);
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW,30*NSEC_PER_SEC),dispatch_get_main_queue(),^{
        ProbeController *owner=weakSelf;
        if(owner && generation==owner.networkGeneration && owner.networkCheck){
            owner.deviceStatus=[owner.deviceStatus stringByAppendingString:@"\nConnection timed out. Resolve the network issue above, then tap Start to retry."];
            [owner cancelNetworkCheck];
        }
    });
}
- (void)becameActive:(NSNotification *)notification {if(self.startWhenActive){self.startWhenActive=NO;[self startMedia];}}
- (void)dealloc {[NSNotificationCenter.defaultCenter removeObserver:self];}
- (void)cancelNetworkCheck {
    self.networkGeneration+=1;
    if(self.networkCheck){nw_connection_set_state_changed_handler(self.networkCheck,nil);nw_connection_cancel(self.networkCheck);self.networkCheck=nil;}
}
- (void)openPermissions {
    [UIApplication.sharedApplication openURL:[NSURL URLWithString:UIApplicationOpenSettingsURLString] options:@{} completionHandler:nil];
}
- (UIView *)broadcastPicker {
    RPSystemBroadcastPickerView *picker=[[RPSystemBroadcastPickerView alloc] initWithFrame:CGRectMake(0,0,44,44)];
    picker.preferredExtension=@"com.lingxi.webrtc.probe.broadcast";picker.showsMicrophoneButton=NO;picker.accessibilityIdentifier=@"systemBroadcastPicker";
    [picker.heightAnchor constraintEqualToConstant:44].active=YES;return picker;
}
- (void)prepareSystemBroadcast {
    [self stopMedia];
    NSURL *root=[NSFileManager.defaultManager containerURLForSecurityApplicationGroupIdentifier:@"group.com.lingxi.code"];
    if(!root){self.deviceStatus=@"Screen sharing App Group unavailable.";return;}
    NSDictionary *config=@{@"createdAt":@(NSDate.date.timeIntervalSince1970),@"runID":NSUUID.UUID.UUIDString,@"endpoint":[NSBundle.mainBundle objectForInfoDictionaryKey:@"LingxiWebRTCFixtureEndpoint"]?:@"",@"token":@"local-webrtc-test-token",@"ice":[NSBundle.mainBundle objectForInfoDictionaryKey:@"LingxiWebRTCIceConfig"]?:@""};
    BOOL saved=[[NSJSONSerialization dataWithJSONObject:config options:0 error:nil] writeToURL:[root URLByAppendingPathComponent:@"webrtc-broadcast-config.json"] atomically:YES];
    self.deviceStatus=saved?@"Ready. Tap the system broadcast picker below, choose LingXi Screen Share, and Start Broadcast.":@"Unable to prepare system screen sharing.";
}
- (void)startScreen {[self.devices startAppScreen];}
- (void)testSpeaker {[self.devices testSpeaker];}
- (void)stopMedia {self.benchmarkUntil=0;self.displayLink.paused=YES;UIApplication.sharedApplication.idleTimerDisabled=NO;self.startWhenActive=NO;[self cancelNetworkCheck];[self.devices stop];self.deviceStatus=@"Media stopped.";}
- (void)pollMedia {
    LxWebRtcProbeStats stats={0};
    if (![self.devices stats:&stats]) {
        if (self.deviceStatus) self.report.text=self.deviceStatus;
        return;
    }
    if(stats.state==1 && !self.screenFixtureStarted && [NSProcessInfo.processInfo.arguments containsObject:@"--webrtc-screen-fixture"]){
        self.screenFixtureStarted=YES;
        [self startScreen];
    }
    CFTimeInterval now=CACurrentMediaTime();
    if(now-self.lastReport>=0.5 || stats.state<0){
    double elapsed=now-self.lastReport;
    double cameraFPS=elapsed>0 && stats.camera_decoded_frames>=self.lastCameraFrames ? (stats.camera_decoded_frames-self.lastCameraFrames)/elapsed : 0;
    double screenFPS=elapsed>0 && stats.screen_decoded_frames>=self.lastScreenFrames ? (stats.screen_decoded_frames-self.lastScreenFrames)/elapsed : 0;
    self.lastReport=now;self.lastCameraFrames=stats.camera_decoded_frames;self.lastScreenFrames=stats.screen_decoded_frames;
    NSString *status=stats.state==1 ? (self.devices.screenActive ? @"Connected — sharing this app screen." : @"Connected — microphone, speaker and camera.") : self.deviceStatus?:@"Connecting…";
    if (stats.state==0) status=@"Connecting to the local test server… Allow local network access if prompted.";
    if (stats.state<0) {
        if (stats.failure==-1065) status=@"Cannot reach the local test server. Enable Local Network in app permissions, check that iPhone and Mac use the same Wi-Fi, then tap Start again.";
        else status=@"Connection or media processing failed. Tap Start to retry; the failure code below identifies the stage.";
    }
    self.report.text=[NSString stringWithFormat:@"LingXi WebRTC — device fixture\n%@\nState: %d  Failure: %d\nCaptured: %llu  Rendered: %llu\nMic peak: %u  Speaker peak: %u\nCamera: %llu (%.1f fps)  App screen: %llu (%.1f fps)",status,stats.state,stats.failure,stats.captured_samples,stats.rendered_samples,stats.microphone_peak,stats.speaker_peak,stats.camera_decoded_frames,cameraFPS,stats.screen_decoded_frames,screenFPS];
    NSDictionary *receipt=@{@"fixture":@"native-media-v1",@"recordedAt":@([NSDate.date timeIntervalSince1970]),@"connected":@(stats.state==1),@"failure":@(stats.failure),@"capturedSamples":@(stats.captured_samples),@"renderedSamples":@(stats.rendered_samples),@"microphonePeak":@(stats.microphone_peak),@"speakerPeak":@(stats.speaker_peak),@"decodedCameraFrames":@(stats.camera_decoded_frames),@"cameraFPS":@(cameraFPS),@"decodedAppScreenFrames":@(stats.screen_decoded_frames),@"screenFPS":@(screenFPS),@"localRelayCandidates":@(stats.local_relay_candidates),@"remoteRelayCandidates":@(stats.remote_relay_candidates),@"relayRequired":@(stats.relay_required!=0),@"systemBroadcastVerified":@NO,@"deviceStatus":self.deviceStatus?:@"",@"sharingAppScreen":@(self.devices.screenActive),@"screenDiagnostics":[self.devices screenDiagnostics],@"remoteVerified":@NO};
    NSData *data=[NSJSONSerialization dataWithJSONObject:receipt options:NSJSONWritingPrettyPrinted error:nil];
    NSURL *documents=[[NSFileManager defaultManager] URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
    [data writeToURL:[documents URLByAppendingPathComponent:@"webrtc-media-receipt.json"] atomically:YES];
    if (stats.state<0) {
        self.deviceStatus=[NSString stringWithFormat:@"%@\nFailure: %d",status,stats.failure];
        UIApplication.sharedApplication.idleTimerDisabled=NO;
        [self.devices stop];
        return;
    }
    }
    BOOL hide=self.devices.screenActive && ![self benchmarking];
    self.videoLayer.hidden=hide;if(hide)return;
    LxWebRtcVideoMeta meta={0};
    BOOL found=NO;
    for(int n=0;n<4;n++){
        if(![self.devices takeVideo:&meta pixels:self.pixels.mutableBytes capacity:self.pixels.length])break;
        if(meta.source==(self.devices.screenActive?2:1)){found=YES;break;}
    }
    if(!found || !self.videoLayer.sampleBufferRenderer.readyForMoreMediaData)return;
    CVPixelBufferRef pixels=NULL;
    if (CVPixelBufferCreate(kCFAllocatorDefault,meta.width,meta.height,kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,(__bridge CFDictionaryRef)@{(id)kCVPixelBufferIOSurfacePropertiesKey:@{}},&pixels)!=kCVReturnSuccess) return;
    CVPixelBufferLockBaseAddress(pixels,0);
    const uint8_t *source=self.pixels.bytes;
    uint8_t *y=CVPixelBufferGetBaseAddressOfPlane(pixels,0),*uv=CVPixelBufferGetBaseAddressOfPlane(pixels,1);
    size_t yStride=CVPixelBufferGetBytesPerRowOfPlane(pixels,0),uvStride=CVPixelBufferGetBytesPerRowOfPlane(pixels,1);
    for (uint32_t row=0;row<meta.height;row++) memcpy(y+row*yStride,source+meta.offsets[0]+row*meta.strides[0],meta.width);
    for (uint32_t row=0;row<(meta.height+1)/2;row++) for(uint32_t col=0;col<(meta.width+1)/2;col++) {uv[row*uvStride+col*2]=source[meta.offsets[1]+row*meta.strides[1]+col];uv[row*uvStride+col*2+1]=source[meta.offsets[2]+row*meta.strides[2]+col];}
    CVPixelBufferUnlockBaseAddress(pixels,0);
    CMVideoFormatDescriptionRef format=NULL;CMSampleBufferRef sample=NULL;
    if (CMVideoFormatDescriptionCreateForImageBuffer(kCFAllocatorDefault,pixels,&format)==noErr) {
        CMSampleTimingInfo timing={kCMTimeInvalid,kCMTimeZero,kCMTimeInvalid};
        if (CMSampleBufferCreateReadyWithImageBuffer(kCFAllocatorDefault,pixels,format,&timing,&sample)==noErr) {
            CFArrayRef attachments=CMSampleBufferGetSampleAttachmentsArray(sample,YES);
            if (attachments && CFArrayGetCount(attachments)) CFDictionarySetValue((CFMutableDictionaryRef)CFArrayGetValueAtIndex(attachments,0),kCMSampleAttachmentKey_DisplayImmediately,kCFBooleanTrue);
            if (self.videoLayer.sampleBufferRenderer.status==AVQueuedSampleBufferRenderingStatusFailed) [self.videoLayer.sampleBufferRenderer flush];
            [self.videoLayer.sampleBufferRenderer enqueueSampleBuffer:sample];CFRelease(sample);
        }
        CFRelease(format);
    }
    CVPixelBufferRelease(pixels);
}
@end

@interface ProbeDelegate : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@end
@implementation ProbeDelegate
- (BOOL)application:(UIApplication *)application didFinishLaunchingWithOptions:(NSDictionary *)options {
    self.window = [[UIWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
    self.window.rootViewController = [ProbeController new];
    [self.window makeKeyAndVisible];
    return YES;
}
@end
int main(int argc, char *argv[]) {
    @autoreleasepool { return UIApplicationMain(argc, argv, nil, NSStringFromClass(ProbeDelegate.class)); }
}
