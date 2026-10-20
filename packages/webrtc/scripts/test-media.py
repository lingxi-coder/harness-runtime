#!/usr/bin/env python3
"""Run native media tests with only the declared plugins, without host scanning."""
import os, pathlib, subprocess, tempfile
root=pathlib.Path(__file__).resolve().parents[1]
plugins=pathlib.Path(subprocess.check_output(['pkg-config','--variable=pluginsdir','gstreamer-1.0'],text=True).strip())
names=['app','audioconvert','audioresample','coreelements','opus','rtp','rtpmanager','vpx','videoconvertscale']
with tempfile.TemporaryDirectory(prefix='lingxi-webrtc-media-') as temp:
    directory=pathlib.Path(temp)/'plugins'; directory.mkdir()
    for name in names:
        source=plugins/('libgst'+name+('.dylib' if os.uname().sysname=='Darwin' else '.so'))
        if not source.is_file(): raise SystemExit('Required media plugin missing: '+str(source))
        (directory/source.name).symlink_to(source)
    env=os.environ.copy()
    env['LINGXI_MEDIA_TEST_PLUGIN_DIR']=str(directory)
    for suffix in ['', '_1_0']:
        env['GST_PLUGIN_PATH'+suffix]=''
        env['GST_PLUGIN_SYSTEM_PATH'+suffix]=str(directory)
        env['GST_REGISTRY'+suffix]=str(pathlib.Path(temp)/'registry.bin')
    subprocess.run(['cargo','test','--manifest-path',str(root/'Cargo.toml'),'--locked','--offline','-p','lingxi-webrtc-media','-p','lingxi-webrtc-mobile','--features','desktop-devices'],env=env,check=True)
