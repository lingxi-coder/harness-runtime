package com.lingxi.webrtc.probe;

/** Host-run regression, no Android runtime or artificial device thermal override. */
public final class ThermalVideoPolicyTest {
    private static void expect(int expected,int actual){if(expected!=actual)throw new AssertionError("expected "+expected+", got "+actual);}
    public static void main(String[] args){
        ThermalVideoPolicy p=new ThermalVideoPolicy();
        expect(30,p.update(0,0));expect(30,p.update(1,10));
        expect(15,p.update(2,20));expect(15,p.update(1,21));
        expect(15,p.update(1,30_020));
        // A new hot event cancels the cooldown rather than accumulating cool time.
        expect(15,p.update(2,30_021));expect(15,p.update(0,40_000));
        expect(15,p.update(1,69_999));expect(30,p.update(1,70_000));
        expect(15,p.update(3,70_001));expect(15,p.update(0,80_000));
        expect(30,p.update(0,110_000));
        System.out.println("PASS: thermal admission and continuous cooldown hysteresis");
    }
}
