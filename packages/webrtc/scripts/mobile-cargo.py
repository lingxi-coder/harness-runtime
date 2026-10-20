#!/usr/bin/env python3
"""Use an explicit official SDK for shared Rust media; never probe host libraries.

mobile-cargo.py android --sdk /sdk/arm64 --ndk /path/to/ndk -- build ...
mobile-cargo.py ios --sdk /sdk/GStreamer.xcframework/ios-arm64 -- build ...
The iOS application must link the XCFramework once. Rust rlibs reference its
archive without bundling a copy into every gstreamer/glib dependency.
"""
import argparse
import os
import pathlib
import re
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('platform', choices=['android', 'ios'])
parser.add_argument('--sdk', type=pathlib.Path, required=True)
parser.add_argument('--ndk', type=pathlib.Path)
args, command = parser.parse_known_args()
if command[:1] == ['--']:
    command = command[1:]
if not command:
    parser.error('provide Cargo arguments after --')
sdk = args.sdk.resolve(strict=True)
env = os.environ.copy()
env['PKG_CONFIG_ALLOW_CROSS'] = '1'
env['PKG_CONFIG_PATH'] = ''
env['CARGO_PROFILE_DEV_DEBUG'] = '0'
env['CARGO_PROFILE_DEV_INCREMENTAL'] = 'false'
# Real-time DSP must not run unoptimized iterator/SIMD precondition checks on
# device. Native UI remains debuggable; host regression tests retain assertions.
env['CARGO_PROFILE_DEV_OPT_LEVEL'] = '2'
env['CARGO_PROFILE_DEV_DEBUG_ASSERTIONS'] = 'false'
root = pathlib.Path(__file__).resolve().parents[1]
plugins = ['gstapp', 'gstaudioconvert', 'gstaudioresample', 'gstcoreelements',
           'gstopus', 'gstrtp', 'gstrtpmanager', 'gstvpx', 'gstvideoconvertscale']
with tempfile.TemporaryDirectory(prefix='lingxi-mobile-gst-pc-') as temp:
    if args.platform == 'android':
        if not args.ndk:
            parser.error('Android requires --ndk')
        toolchain = args.ndk.resolve(strict=True) / 'toolchains/llvm/prebuilt/darwin-x86_64/bin'
        compiler = toolchain / 'aarch64-linux-android26-clang'
        if not compiler.is_file():
            parser.error('macOS Android NDK toolchain missing')
        env['CC_aarch64_linux_android'] = str(compiler)
        env['AR_aarch64_linux_android'] = str(toolchain / 'llvm-ar')
        env['CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER'] = str(compiler)
        env['PKG_CONFIG_LIBDIR'] = os.pathsep.join(str(sdk / p) for p in ['lib/pkgconfig', 'lib/gstreamer-1.0/pkgconfig'])
        env['PKG_CONFIG_ALL_STATIC'] = '1'
        target = 'aarch64-linux-android'
    else:
        if not (sdk / 'libGStreamer.a').is_file() or not (sdk / 'Headers/glib.h').is_file():
            parser.error('provide the ios-arm64 XCFramework slice')
        target = 'aarch64-apple-ios'
        env['IPHONEOS_DEPLOYMENT_TARGET'] = '18.0'
        env['PKG_CONFIG_LIBDIR'] = temp
        env.pop('PKG_CONFIG_ALL_STATIC', None)
        modules = ['glib-2.0', 'gobject-2.0', 'gio-2.0', 'gmodule-2.0',
                   'gstreamer-1.0', 'gstreamer-base-1.0', 'gstreamer-app-1.0',
                   'gstreamer-audio-1.0', 'gstreamer-video-1.0'] + plugins
        glib_header = (sdk/'Headers/arm64/glibconfig.h').read_text()
        gst_header = (sdk/'Headers/gst/gstversion.h').read_text()
        glib_version = '.'.join(re.search(r'#define GLIB_'+part+r'_VERSION\s+(\d+)', glib_header).group(1) for part in ['MAJOR','MINOR','MICRO'])
        gst_version = '.'.join(re.search(r'#define GST_VERSION_'+part+r'\s+\((\d+)\)', gst_header).group(1) for part in ['MAJOR','MINOR','MICRO'])
        for module in modules:
            version = glib_version if module in modules[:4] else gst_version
            pathlib.Path(temp, module + '.pc').write_text(
                f'prefix={sdk}\nlibdir=${{prefix}}\nincludedir=${{prefix}}/Headers\n'
                f'Name: {module}\nDescription: Official iPhone GStreamer static SDK\nVersion: {version}\n'
                'Libs: -L${libdir} -lGStreamer\nCflags: -I${includedir}\n')
            # All sys crates share one archive. Only media/build.rs declares
            # its link, without copying the SDK into each dependency rlib.
            name = module.upper().replace('-', '_').replace('.', '_')
            env['SYSTEM_DEPS_' + name + '_LIB'] = ''
            env['SYSTEM_DEPS_' + name + '_LINK'] = 'dynamic'
    subprocess.run(['cargo', *command, '--manifest-path', str(root/'Cargo.toml'),
                    '--target', target], env=env, check=True)
