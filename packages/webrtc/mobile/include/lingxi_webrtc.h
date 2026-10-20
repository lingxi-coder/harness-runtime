#ifndef LINGXI_WEBRTC_H
#define LINGXI_WEBRTC_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
// Background worker only. Positive Opus packet count on success; negative error.
int32_t lingxi_webrtc_verify_codecs(void);
// Standalone diagnostic host. Product code embeds core/media on its own runtime.
typedef struct LxWebRtcProbe LxWebRtcProbe;
typedef struct {
    int32_t state, failure;
    uint64_t captured_samples, rendered_samples, decoded_video_frames;
    uint32_t microphone_peak, speaker_peak;
    uint64_t camera_decoded_frames, screen_decoded_frames;
    uint32_t local_relay_candidates, remote_relay_candidates, relay_required;
    uint64_t encoded_camera_frames, encoded_screen_frames;
} LxWebRtcProbeStats;
typedef struct { const uint8_t *data; size_t len, row_stride, pixel_stride; } LxWebRtcPlane;
typedef struct {
    uint32_t width, height, ssrc, source;
    size_t size, offsets[3];
    int32_t strides[3];
} LxWebRtcVideoMeta;
uint64_t lingxi_webrtc_clock_ns(void);
// ICE JSON is borrowed for this call, <=64 KiB; NULL/0 selects direct ICE.
LxWebRtcProbe *lingxi_webrtc_probe_open(const char *endpoint, const char *token, uint32_t capture_rate, uint32_t render_rate, const uint8_t *ice_json, size_t ice_len);
// Video-only extension host; no native audio/APM resources or decoded preview.
LxWebRtcProbe *lingxi_webrtc_broadcast_open(const char *endpoint, const char *token, const uint8_t *ice_json, size_t ice_len);
// Stop all callbacks first; close may join a worker and must run off the UI thread.
void lingxi_webrtc_probe_close(LxWebRtcProbe *probe);
// Enable microphone=1, speaker=2, camera=4, screen=8, receive video=16.
// Bit 32 requests a finite quiet speaker test; microphone is never echoed back.
void lingxi_webrtc_probe_controls(const LxWebRtcProbe *probe, uint32_t bits);
int32_t lingxi_webrtc_probe_stats(const LxWebRtcProbe *probe, LxWebRtcProbeStats *out);
// Borrowed buffers stay valid until the call returns. One capture and one render
// caller may run concurrently. They never wait for encoding or network I/O.
int32_t lingxi_webrtc_probe_capture(const LxWebRtcProbe *probe, const float *pcm, size_t count, uint64_t captured_ns, uint64_t callback_ns);
int32_t lingxi_webrtc_probe_render(const LxWebRtcProbe *probe, float *pcm, size_t count, uint64_t playback_ns);
int32_t lingxi_webrtc_probe_video(const LxWebRtcProbe *probe, uint32_t source, uint32_t width, uint32_t height, const LxWebRtcPlane planes[3], uint64_t captured_ns);
int32_t lingxi_webrtc_probe_rgba(const LxWebRtcProbe *probe, uint32_t source, uint32_t width, uint32_t height, LxWebRtcPlane plane, int32_t bgra, uint64_t captured_ns);
int32_t lingxi_webrtc_probe_take_video(const LxWebRtcProbe *probe, LxWebRtcVideoMeta *out, uint8_t *pixels, size_t capacity);
#ifdef __cplusplus
}
#endif
#endif
