#include <jni.h>
#include <android/bitmap.h>
#include <cstring>
#include "lingxi_webrtc.h"
#define JNI_FN(name) extern "C" JNIEXPORT name JNICALL
static LxWebRtcProbe *probe(jlong handle) { return reinterpret_cast<LxWebRtcProbe *>(handle); }
static uint8_t *buffer(JNIEnv *env, jobject value, jint offset, jint length) {
    if (!value || offset < 0 || length < 0) return nullptr;
    jlong capacity=env->GetDirectBufferCapacity(value);
    if (capacity<0 || static_cast<jlong>(offset)+length>capacity) return nullptr;
    auto *data=static_cast<uint8_t *>(env->GetDirectBufferAddress(value));
    return data ? data+offset : nullptr;
}
JNI_FN(jint) Java_com_lingxi_webrtc_probe_NativeBridge_verifyCodecs(JNIEnv *,jclass) {return lingxi_webrtc_verify_codecs();}
JNI_FN(jlong) Java_com_lingxi_webrtc_probe_NativeBridge_clockNs(JNIEnv *,jclass) {return lingxi_webrtc_clock_ns();}
JNI_FN(jlong) Java_com_lingxi_webrtc_probe_NativeBridge_open(JNIEnv *env,jclass,jstring endpoint,jstring token,jint capture,jint render,jbyteArray iceJson) {
    if (!endpoint || !token) return 0;
    const char *address=env->GetStringUTFChars(endpoint,nullptr);
    if (!address) return 0;
    const char *secret=env->GetStringUTFChars(token,nullptr);
    jsize iceLength=iceJson?env->GetArrayLength(iceJson):0;
    jbyte *ice=iceJson && iceLength<=65536?env->GetByteArrayElements(iceJson,nullptr):nullptr;
    auto *handle=secret && (!iceJson || ice) ? lingxi_webrtc_probe_open(address,secret,capture,render,reinterpret_cast<const uint8_t *>(ice),iceLength) : nullptr;
    if(ice)env->ReleaseByteArrayElements(iceJson,ice,JNI_ABORT);
    if (secret) env->ReleaseStringUTFChars(token,secret);
    env->ReleaseStringUTFChars(endpoint,address);
    return reinterpret_cast<jlong>(handle);
}
JNI_FN(void) Java_com_lingxi_webrtc_probe_NativeBridge_close(JNIEnv *,jclass,jlong handle) {lingxi_webrtc_probe_close(probe(handle));}
JNI_FN(void) Java_com_lingxi_webrtc_probe_NativeBridge_controls(JNIEnv *,jclass,jlong handle,jint bits) {lingxi_webrtc_probe_controls(probe(handle),bits);}
JNI_FN(jlongArray) Java_com_lingxi_webrtc_probe_NativeBridge_stats(JNIEnv *env,jclass,jlong handle) {
    LxWebRtcProbeStats stats{};
    if (lingxi_webrtc_probe_stats(probe(handle),&stats)) return nullptr;
    jlong values[]={stats.state,stats.failure,static_cast<jlong>(stats.captured_samples),static_cast<jlong>(stats.rendered_samples),static_cast<jlong>(stats.decoded_video_frames),stats.microphone_peak,stats.speaker_peak,static_cast<jlong>(stats.camera_decoded_frames),static_cast<jlong>(stats.screen_decoded_frames),stats.local_relay_candidates,stats.remote_relay_candidates,stats.relay_required};
    auto result=env->NewLongArray(12);if(result)env->SetLongArrayRegion(result,0,12,values);return result;
}
JNI_FN(jint) Java_com_lingxi_webrtc_probe_NativeBridge_capture(JNIEnv *env,jclass,jlong handle,jobject pcm,jint count,jlong captured,jlong callback) {
    if(count<0 || count>8192)return -1;
    auto *data=buffer(env,pcm,0,count*4);if(!data)return -1;
    return lingxi_webrtc_probe_capture(probe(handle),reinterpret_cast<float *>(data),count,captured,callback);
}
JNI_FN(jint) Java_com_lingxi_webrtc_probe_NativeBridge_render(JNIEnv *env,jclass,jlong handle,jobject pcm,jint count,jlong playback) {
    if(count<0 || count>8192)return -1;
    auto *data=buffer(env,pcm,0,count*4);if(!data)return -1;
    return lingxi_webrtc_probe_render(probe(handle),reinterpret_cast<float *>(data),count,playback);
}
JNI_FN(jint) Java_com_lingxi_webrtc_probe_NativeBridge_video(JNIEnv *env,jclass,jlong handle,jint source,jint width,jint height,jobject y,jint yo,jint yl,jint yr,jint yp,jobject u,jint uo,jint ul,jint ur,jint up,jobject v,jint vo,jint vl,jint vr,jint vp,jlong at) {
    if(yr<=0 || yp<=0 || ur<=0 || up<=0 || vr<=0 || vp<=0)return -1;
    LxWebRtcPlane planes[]={{buffer(env,y,yo,yl),static_cast<size_t>(yl),static_cast<size_t>(yr),static_cast<size_t>(yp)},{buffer(env,u,uo,ul),static_cast<size_t>(ul),static_cast<size_t>(ur),static_cast<size_t>(up)},{buffer(env,v,vo,vl),static_cast<size_t>(vl),static_cast<size_t>(vr),static_cast<size_t>(vp)}};
    return lingxi_webrtc_probe_video(probe(handle),source,width,height,planes,at);
}
JNI_FN(jint) Java_com_lingxi_webrtc_probe_NativeBridge_rgba(JNIEnv *env,jclass,jlong handle,jint source,jint width,jint height,jobject pixels,jint offset,jint length,jint stride,jint pixelStride,jlong at) {
    if(stride<=0 || pixelStride<4)return -1;
    LxWebRtcPlane plane{buffer(env,pixels,offset,length),static_cast<size_t>(length),static_cast<size_t>(stride),static_cast<size_t>(pixelStride)};
    return lingxi_webrtc_probe_rgba(probe(handle),source,width,height,plane,0,at);
}
JNI_FN(jintArray) Java_com_lingxi_webrtc_probe_NativeBridge_takeVideo(JNIEnv *env,jclass,jlong handle,jobject pixels) {
    jlong capacity=env->GetDirectBufferCapacity(pixels);
    auto *data=static_cast<uint8_t *>(env->GetDirectBufferAddress(pixels));
    if(capacity<=0 || !data)return nullptr;
    LxWebRtcVideoMeta meta{};
    if(lingxi_webrtc_probe_take_video(probe(handle),&meta,data,capacity)!=1)return nullptr;
    jint values[]={static_cast<jint>(meta.width),static_cast<jint>(meta.height),static_cast<jint>(meta.ssrc),static_cast<jint>(meta.source),static_cast<jint>(meta.size),static_cast<jint>(meta.offsets[0]),static_cast<jint>(meta.offsets[1]),static_cast<jint>(meta.offsets[2]),meta.strides[0],meta.strides[1],meta.strides[2]};
    auto result=env->NewIntArray(11);if(result)env->SetIntArrayRegion(result,0,11,values);return result;
}
static uint8_t clamp(int value) {return value<0 ? 0 : (value>255 ? 255 : value);}
JNI_FN(void) Java_com_lingxi_webrtc_probe_NativeBridge_renderBitmap(JNIEnv *env,jclass,jobject pixels,jintArray metadata,jobject bitmap) {
    if(!metadata || env->GetArrayLength(metadata)!=11)return;
    jint meta[11];env->GetIntArrayRegion(metadata,0,11,meta);
    auto *data=buffer(env,pixels,0,meta[4]);if(!data)return;
    AndroidBitmapInfo info{};void *output=nullptr;
    if(AndroidBitmap_getInfo(env,bitmap,&info)!=ANDROID_BITMAP_RESULT_SUCCESS || info.format!=ANDROID_BITMAP_FORMAT_RGBA_8888 || info.width!=meta[0] || info.height!=meta[1] || AndroidBitmap_lockPixels(env,bitmap,&output)!=ANDROID_BITMAP_RESULT_SUCCESS)return;
    for(uint32_t y=0;y<info.height;y++)for(uint32_t x=0;x<info.width;x++) {
        int iy=meta[5]+y*meta[8]+x,iu=meta[6]+(y/2)*meta[9]+x/2,iv=meta[7]+(y/2)*meta[10]+x/2;
        if(iy<0 || iu<0 || iv<0 || iy>=meta[4] || iu>=meta[4] || iv>=meta[4])continue;
        int c=data[iy]-16,d=data[iu]-128,e=data[iv]-128;
        auto *p=static_cast<uint8_t *>(output)+y*info.stride+x*4;
        p[0]=clamp((298*c+409*e+128)>>8);p[1]=clamp((298*c-100*d-208*e+128)>>8);p[2]=clamp((298*c+516*d+128)>>8);p[3]=255;
    }
    AndroidBitmap_unlockPixels(env,bitmap);
}
