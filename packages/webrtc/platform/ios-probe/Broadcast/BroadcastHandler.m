#import <ReplayKit/ReplayKit.h>
#import "LXWebRTCCapture.h"
#import <mach/mach.h>

@interface LxBroadcastLease:NSObject
@property(nonatomic,assign) LxWebRtcProbe *handle;
@end
@implementation LxBroadcastLease
- (void)dealloc {LxWebRtcProbe *handle=_handle;if(handle)dispatch_async(dispatch_get_global_queue(QOS_CLASS_UTILITY,0),^{lingxi_webrtc_probe_close(handle);});}
@end

@interface LxWebRTCBroadcastHandler:RPBroadcastSampleHandler
@property(atomic,strong) LxBroadcastLease *lease;
@property(nonatomic,strong) dispatch_queue_t controlQueue;
@property(nonatomic,strong) dispatch_source_t health;
@property(atomic,assign) NSUInteger generation;
@property(atomic,assign) BOOL paused;
@property(atomic,copy) NSString *phase;
@property(atomic,copy) NSString *runID;
@property(atomic,copy) NSString *applicationID;
@property(atomic,assign) uint64_t callbacks,submitted,lastFrame;
@property(atomic,assign) int32_t submitResult;
@property(atomic,assign) uint32_t width,height;
@end
@implementation LxWebRTCBroadcastHandler
- (instancetype)init {if((self=[super init])){_controlQueue=dispatch_queue_create("lingxi.webrtc.broadcast",DISPATCH_QUEUE_SERIAL);_phase=@"idle";}return self;}
- (NSURL *)sharedRoot {return [NSFileManager.defaultManager containerURLForSecurityApplicationGroupIdentifier:@"group.com.lingxi.code"];}
- (void)receipt {
    LxWebRtcProbeStats stats={0};LxBroadcastLease *lease=self.lease;
    if(lease)lingxi_webrtc_probe_stats(lease.handle,&stats);
    task_vm_info_data_t memory={0};mach_msg_type_number_t count=TASK_VM_INFO_COUNT;
    uint64_t footprint=task_info(mach_task_self(),TASK_VM_INFO,(task_info_t)&memory,&count)==KERN_SUCCESS?memory.phys_footprint:0;
    NSDictionary *value=@{@"recordedAt":@(NSDate.date.timeIntervalSince1970),@"runID":self.runID?:@"",@"phase":self.phase?:@"",@"applicationID":self.applicationID?:@"",@"callbacks":@(self.callbacks),@"submitted":@(self.submitted),@"submitResult":@(self.submitResult),@"width":@(self.width),@"height":@(self.height),@"state":@(stats.state),@"failure":@(stats.failure),@"encodedScreenFrames":@(stats.encoded_screen_frames),@"localRelayCandidates":@(stats.local_relay_candidates),@"remoteRelayCandidates":@(stats.remote_relay_candidates),@"relayRequired":@(stats.relay_required!=0),@"footprintBytes":@(footprint),@"microphoneCaptured":@NO};
    NSURL *root=[self sharedRoot];if(root)[[NSJSONSerialization dataWithJSONObject:value options:0 error:nil] writeToURL:[root URLByAppendingPathComponent:@"webrtc-broadcast-receipt.json"] atomically:YES];
}
- (void)fail:(NSString *)message {
    self.phase=@"failed";[self receipt];
    [self finishBroadcastWithError:[NSError errorWithDomain:@"LingXiWebRTC" code:1 userInfo:@{NSLocalizedDescriptionKey:message}]];
    [self stopSession];
}
- (void)broadcastStartedWithSetupInfo:(NSDictionary<NSString *,NSObject *> *)setupInfo {
    NSUInteger generation=++self.generation;self.phase=@"starting";
    dispatch_async(self.controlQueue,^{
        NSURL *root=[self sharedRoot];NSData *data=root?[NSData dataWithContentsOfURL:[root URLByAppendingPathComponent:@"webrtc-broadcast-config.json"]]:nil;
        id decoded=data.length<=65536?[NSJSONSerialization JSONObjectWithData:data?:NSData.data options:0 error:nil]:nil;
        NSDictionary *config=[decoded isKindOfClass:NSDictionary.class]?decoded:nil;
        NSString *endpoint=[config[@"endpoint"] isKindOfClass:NSString.class]?config[@"endpoint"]:nil;
        NSString *token=[config[@"token"] isKindOfClass:NSString.class]?config[@"token"]:nil;
        NSString *ice=[config[@"ice"] isKindOfClass:NSString.class]?config[@"ice"]:@"";
        self.runID=[config[@"runID"] isKindOfClass:NSString.class]?config[@"runID"]:@"";
        if(!endpoint.length || !token.length || ![config[@"createdAt"] isKindOfClass:NSNumber.class] || NSDate.date.timeIntervalSince1970-[config[@"createdAt"] doubleValue]>600){[self fail:@"Open LingXi and prepare system screen sharing again."];return;}
        if(generation!=self.generation)return;
        NSData *iceData=[ice dataUsingEncoding:NSUTF8StringEncoding];
        LxWebRtcProbe *handle=lingxi_webrtc_broadcast_open(endpoint.UTF8String,token.UTF8String,iceData.bytes,iceData.length);
        if(!handle){[self fail:@"Unable to initialize shared WebRTC broadcast transport."];return;}
        LxBroadcastLease *lease=[LxBroadcastLease new];lease.handle=handle;
        if(generation!=self.generation)return;
        self.lease=lease;self.phase=@"connecting";lingxi_webrtc_probe_controls(handle,8);
        self.health=dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER,0,0,self.controlQueue);
        dispatch_source_set_timer(self.health,DISPATCH_TIME_NOW,NSEC_PER_SEC,NSEC_PER_SEC/10);
        __weak LxWebRTCBroadcastHandler *weakSelf=self;
        dispatch_source_set_event_handler(self.health,^{
            LxWebRTCBroadcastHandler *owner=weakSelf;LxBroadcastLease *current=owner.lease;if(!owner || !current)return;
            LxWebRtcProbeStats stats={0};lingxi_webrtc_probe_stats(current.handle,&stats);
            if(stats.state<0){[owner fail:[NSString stringWithFormat:@"WebRTC broadcast disconnected (%d).",stats.failure]];return;}
            if(stats.state==1 && !owner.paused)owner.phase=@"active";
            [owner receipt];
        });dispatch_resume(self.health);[self receipt];
    });
}
- (void)broadcastPaused {self.paused=YES;self.phase=@"paused";LxBroadcastLease *lease=self.lease;if(lease)lingxi_webrtc_probe_controls(lease.handle,0);[self receipt];}
- (void)broadcastResumed {self.paused=NO;self.lastFrame=0;self.phase=@"active";LxBroadcastLease *lease=self.lease;if(lease)lingxi_webrtc_probe_controls(lease.handle,8);[self receipt];}
- (void)broadcastAnnotatedWithApplicationInfo:(NSDictionary *)info {NSString *identifier=info[RPApplicationInfoBundleIdentifierKey];if([identifier isKindOfClass:NSString.class])self.applicationID=identifier;[self receipt];}
- (void)processSampleBuffer:(CMSampleBufferRef)sample withType:(RPSampleBufferType)type {
    if(type!=RPSampleBufferTypeVideo || self.paused)return;
    self.callbacks+=1;LxBroadcastLease *lease=self.lease;if(!lease)return;
    uint64_t now=lingxi_webrtc_clock_ns(),period=1000000000/30;
    if(now+period/2<self.lastFrame)return;
    self.lastFrame=self.lastFrame && now-self.lastFrame<period?self.lastFrame+period:now+period;
    CVPixelBufferRef pixels=CMSampleBufferGetImageBuffer(sample);if(!pixels)return;
    self.width=(uint32_t)CVPixelBufferGetWidth(pixels);self.height=(uint32_t)CVPixelBufferGetHeight(pixels);
    self.submitResult=LXWebRTCSubmitVideo(lease.handle,pixels,CMSampleBufferGetPresentationTimeStamp(sample),2);
    if(self.submitResult==0)self.submitted+=1;
}
- (void)stopSession {self.generation+=1;LxBroadcastLease *lease=self.lease;if(lease)lingxi_webrtc_probe_controls(lease.handle,0);self.lease=nil;if(self.health){dispatch_source_cancel(self.health);self.health=nil;}}
- (void)broadcastFinished {if(![self.phase isEqualToString:@"failed"])self.phase=@"finished";[self receipt];[self stopSession];}
@end
