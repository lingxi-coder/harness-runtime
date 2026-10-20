#import <AVFoundation/AVFoundation.h>
#import "lingxi_webrtc.h"
@interface LxNativeDevices : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
@property(nonatomic, copy) void (^status)(NSString *message);
@property(atomic, readonly) BOOL screenActive;
- (void)startWithEndpoint:(NSString *)endpoint token:(NSString *)token;
- (void)startAppScreen;
- (void)testSpeaker;
- (NSDictionary *)screenDiagnostics;
- (void)stop;
- (BOOL)stats:(LxWebRtcProbeStats *)stats;
- (BOOL)takeVideo:(LxWebRtcVideoMeta *)meta pixels:(uint8_t *)pixels capacity:(size_t)capacity;
@end
