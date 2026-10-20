#import "LXWebRTCCapture.h"
#include <math.h>
static uint64_t CaptureTime(CMTime pts) {
    double age = CMTimeGetSeconds(CMTimeSubtract(CMClockGetTime(CMClockGetHostTimeClock()), pts));
    uint64_t now = lingxi_webrtc_clock_ns();
    if (!isfinite(age) || age < -0.001 || age > 1.) return UINT64_MAX;
    uint64_t nanos = (uint64_t)(MAX(0., age) * 1e9);
    return nanos <= now ? now - nanos : UINT64_MAX;
}
int32_t LXWebRTCSubmitVideo(LxWebRtcProbe *handle, CVPixelBufferRef pixels, CMTime pts, uint32_t source) {
    uint64_t captured = CaptureTime(pts);
    if (!handle || captured == UINT64_MAX) return -101;
    if (CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return -102;
    int32_t result=-103;
    OSType format = CVPixelBufferGetPixelFormatType(pixels);
    uint32_t width = (uint32_t)CVPixelBufferGetWidth(pixels), height = (uint32_t)CVPixelBufferGetHeight(pixels);
    if ((format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange || format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) && CVPixelBufferGetPlaneCount(pixels) == 2) {
        uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(pixels, 0), *uv = CVPixelBufferGetBaseAddressOfPlane(pixels, 1);
        size_t yStride = CVPixelBufferGetBytesPerRowOfPlane(pixels, 0), uvStride = CVPixelBufferGetBytesPerRowOfPlane(pixels, 1);
        size_t yLen = yStride * CVPixelBufferGetHeightOfPlane(pixels, 0), uvLen = uvStride * CVPixelBufferGetHeightOfPlane(pixels, 1);
        LxWebRtcPlane planes[3] = {{y,yLen,yStride,1},{uv,uvLen,uvStride,2},{uv+1,uvLen-1,uvStride,2}};
        result=lingxi_webrtc_probe_video(handle, source, width, height, planes, captured);
    } else if (format == kCVPixelFormatType_32BGRA) {
        size_t stride = CVPixelBufferGetBytesPerRow(pixels);
        LxWebRtcPlane plane = {CVPixelBufferGetBaseAddress(pixels),stride*height,stride,4};
        result=lingxi_webrtc_probe_rgba(handle,source,width,height,plane,1,captured);
    }
    CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    return result;
}
