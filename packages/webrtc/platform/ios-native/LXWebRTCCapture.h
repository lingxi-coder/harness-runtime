#import <AVFoundation/AVFoundation.h>
#import "lingxi_webrtc.h"
// Borrowed CVPixelBuffer stays locked only for the synchronous shared-Rust copy.
int32_t LXWebRTCSubmitVideo(LxWebRtcProbe *handle,CVPixelBufferRef pixels,CMTime pts,uint32_t source);
