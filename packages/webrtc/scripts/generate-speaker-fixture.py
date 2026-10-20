#!/usr/bin/env python3
"""Generate a quiet, finite speaker fixture with the platform libopus codec.

No recorded microphone data. 200 Hz at peak 0.01 (-40 dBFS), one second,
80 ms cosine fades, 50 mono 48 kHz / 20 ms Opus packets. Output is committed
so the Docker fixture needs neither a codec runtime nor a model/network service.
"""
import ctypes as c
import math
import pathlib
import struct
import subprocess
import sys

libdir=pathlib.Path(subprocess.check_output(['pkg-config','--variable=libdir','opus'],text=True).strip())
opus=c.CDLL(str(libdir/('libopus.dylib' if sys.platform=='darwin' else 'libopus.so')))
opus.opus_encoder_create.argtypes=[c.c_int,c.c_int,c.c_int,c.POINTER(c.c_int)]
opus.opus_encoder_create.restype=c.c_void_p
opus.opus_encode_float.argtypes=[c.c_void_p,c.POINTER(c.c_float),c.c_int,c.POINTER(c.c_ubyte),c.c_int]
opus.opus_encode_float.restype=c.c_int
opus.opus_encoder_destroy.argtypes=[c.c_void_p]
error=c.c_int()
encoder=opus.opus_encoder_create(48000,1,2048,c.byref(error))
if not encoder or error.value: raise RuntimeError('Opus encoder initialization failed')
data=bytearray(b'LXOP')
try:
    for frame in range(50):
        samples=[]
        for n in range(960):
            position=(frame*960+n)/48000
            fade=min(1.,position/.08,(1.-position)/.08)
            envelope=.5-.5*math.cos(math.pi*max(0.,fade))
            samples.append(.01*envelope*math.sin(2*math.pi*200*position))
        pcm=(c.c_float*960)(*samples);packet=(c.c_ubyte*1275)()
        length=opus.opus_encode_float(encoder,pcm,960,packet,1275)
        if length<=0: raise RuntimeError('Opus encoding failed')
        data.extend(struct.pack('<H',length));data.extend(bytes(packet[:length]))
finally:
    opus.opus_encoder_destroy(encoder)
output=pathlib.Path(__file__).resolve().parents[1]/'test-server/fixtures/speaker-200hz.opus-packets'
output.parent.mkdir(parents=True,exist_ok=True);output.write_bytes(data)
opus.opus_decoder_create.argtypes=[c.c_int,c.c_int,c.POINTER(c.c_int)]
opus.opus_decoder_create.restype=c.c_void_p
opus.opus_decode_float.argtypes=[c.c_void_p,c.POINTER(c.c_ubyte),c.c_int,c.POINTER(c.c_float),c.c_int,c.c_int]
opus.opus_decode_float.restype=c.c_int
opus.opus_decoder_destroy.argtypes=[c.c_void_p]
decoder=opus.opus_decoder_create(48000,1,c.byref(error))
if not decoder or error.value: raise RuntimeError('Opus decoder initialization failed')
at=4;peak=0.;frames=0
try:
    while at<len(data):
        length=struct.unpack_from('<H',data,at)[0];at+=2
        packet=(c.c_ubyte*length).from_buffer_copy(data[at:at+length]);at+=length
        pcm=(c.c_float*960)()
        if opus.opus_decode_float(decoder,packet,length,pcm,960,0)!=960:raise RuntimeError('Unexpected fixture frame duration')
        peak=max(peak,max(abs(x) for x in pcm));frames+=1
    if frames!=50 or not 0<peak<.02: raise RuntimeError('Fixture is not quiet and finite')
finally:
    opus.opus_decoder_destroy(decoder)
print(f'Verified one second / 50 packets; decoded peak {peak:.5f} (< 0.02)')
print(f'Generated {len(data)} bytes: {output}')
