package com.lingxi.webrtc.probe;
import android.graphics.Bitmap;
import java.nio.ByteBuffer;
final class NativeBridge {
    static { System.loadLibrary("lingxi_webrtc_probe"); }
    static native int verifyCodecs();
    static native long clockNs();
    static native long open(String endpoint, String token, int captureRate, int renderRate, byte[] iceJson);
    static native void close(long handle);
    static native void controls(long handle, int bits);
    static native long[] stats(long handle);
    static native int capture(long handle, ByteBuffer pcm, int count, long capturedNs, long callbackNs);
    static native int render(long handle, ByteBuffer pcm, int count, long playbackNs);
    static native int video(long handle, int source, int width, int height, ByteBuffer y, int yOffset, int yLength, int yRow, int yPixel, ByteBuffer u, int uOffset, int uLength, int uRow, int uPixel, ByteBuffer v, int vOffset, int vLength, int vRow, int vPixel, long at);
    static native int rgba(long handle, int source, int width, int height, ByteBuffer pixels, int offset, int length, int stride, int pixelStride, long at);
    static native int[] takeVideo(long handle, ByteBuffer pixels);
    static native void renderBitmap(ByteBuffer pixels, int[] meta, Bitmap bitmap);
}
