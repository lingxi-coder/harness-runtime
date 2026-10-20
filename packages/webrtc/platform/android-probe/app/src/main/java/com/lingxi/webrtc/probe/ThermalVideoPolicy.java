package com.lingxi.webrtc.probe;

/** Capture admission policy; timestamps and cooldown use a monotonic clock. */
final class ThermalVideoPolicy {
    private int fps=30;
    private long coolSince=-1;
    int update(int thermalStatus,long nowMs){
        if(thermalStatus>=2){fps=15;coolSince=-1;}
        else if(fps<30){
            if(coolSince<0)coolSince=nowMs;
            if(nowMs-coolSince>=30_000){fps=30;coolSince=-1;}
        }
        return fps;
    }
}
