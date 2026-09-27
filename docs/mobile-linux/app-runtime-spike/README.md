# local-apps phase-0 spike: Node + Next.js dev under the Alpine PRoot runtime

> Historical measurement fixture only. Production pins, the production-only
> runtime command contract, SBOM, and static-export-compatible scaffold now
> live in `docs/mobile-linux/local-app-runtime-pins.json`,
> `docs/mobile-linux/local-app-runtime-policy.json`, and
> `crates/local-apps/templates/next-static-v1`. Never use the placeholder
> spike manifest as a release input.

This directory is a **manual feasibility spike kit**, not product code. It
answers one question with numbers from a real device: *can the existing
Alpine 3.21.3 PRoot runtime (see `docs/mobile-linux/rootfs/README.md` and
`mobile-linux-pins.json`) run a Node + Next.js 16 development workflow within
the local-apps V1 acceptance targets?*

Ground rules, matching the MobileLinux rootfs pipeline conventions:

- **The device is offline.** Every external artifact is fetched on the Mac,
  digest-verified, and pushed over `adb`. No script run on the device or in
  the guest performs any network access beyond `127.0.0.1`.
- **Every external artifact is pinned** (URL + sha256) in
  [`spike-pins.json`](spike-pins.json). Placeholder values follow the
  `REPLACE_WITH_REAL_*` convention from `rootfs-manifest.sample.json` and are
  filled in on the first staging run.
- **npm and corepack are never installed on the device.** The guest gets the
  Alpine `nodejs` runtime only; the entire `node_modules` tree is materialized
  on the Mac and pushed as a tarball. Staging and push both hard-fail if an
  `npm`/`corepack`/`yarn`/`pnpm` package or binary shows up.
- **No Gradle integration.** Everything below is `adb` + shell, run by a
  human. The product runtime integration is a later phase.

Execution contexts (each scripts/ subdirectory targets exactly one):

| Directory | Runs where | Shell |
| --- | --- | --- |
| `scripts/mac/` | macOS host | bash (shellcheck-clean) |
| `scripts/device/` | Android adb shell | POSIX sh (mksh/toybox) |
| `scripts/guest/` | Alpine guest under PRoot | POSIX sh (BusyBox 1.37 ash) |

## Prerequisites

Mac:

- `adb` (platform-tools), `curl`, `python3`, `shasum`
- Node.js >= 20 with npm >= 10 (only to materialize `node_modules`; the exact
  Mac node version is not shipped anywhere)
- Docker Desktop — **pin time only**, to resolve the Alpine apk dependency
  closure for `nodejs` with `apk fetch --recursive`
- PRoot binaries built from the pinned fork (one-time):

  ```sh
  clients/android/scripts/build-mobile-linux-native.sh --variant direct
  ```

Device:

- Real `arm64-v8a` hardware, Android 10+ (validated target: the Android 16
  device from `../openminis-android-capability-report.md`), USB debugging on
- >= 3 GiB free under `/data/local/tmp`
- No network needed at any point

Why `/data/local/tmp` + `adb shell`: the capability report showed Android 16
blocks `execve()` of writable files for `untrusted_app` processes, while the
`shell` domain may exec from `/data/local/tmp`. A phase-0 spike measures the
runtime cost, not the app-packaging story (that is the read-only
`nativeLibraryDir` loader path, already solved for PRoot itself).

## Artifact provenance

Why Alpine apk packages are the Node source (and not nodejs.org binaries,
which are glibc-only): see the sourcing decision in
[`node-sourcing-memo.md`](node-sourcing-memo.md). This kit uses its
recommended option (a); expect `nodejs` 22.x from v3.21 main at pin time.

Every byte that reaches the device comes from one of these sources. `sha256`
column: `pinned` = real digest in `spike-pins.json`, `placeholder` = to be
filled from `spike-pins.candidate.json` on the first staging run.

| Artifact | Source | sha256 | Staged how |
| --- | --- | --- | --- |
| Alpine minirootfs 3.21.3 aarch64 | `https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/aarch64/alpine-minirootfs-3.21.3-aarch64.tar.gz` | pinned (`ead8a4b3…`, inherited from `mobile-linux-pins.json`) | curl on Mac → adb push → device-side `sha256sum` re-check → `tar -xzf` on device (toybox tar preserves the busybox applet symlinks; `adb push` of an extracted tree would not) |
| PRoot + unbundled loader | built locally by `build-mobile-linux-native.sh` from the pinned OpenMinis fork (`mobile-linux-pins.json` components) — never downloaded | pinned via source commits; ELF `e_machine==183` checked at pack | copied from `clients/android/app/src/<variant>/jniLibs/arm64-v8a/{libproot.so,libproot-loader.so}` → `adb push` as `bin/proot`, `bin/loader` |
| Node runtime (`nodejs` apk + shared-library closure) | Alpine v3.21 aarch64 repos, `https://dl-cdn.alpinelinux.org/alpine/v3.21/<repo>/aarch64/<name>-<version>.apk`; closure resolved once via `docker run --platform linux/arm64 alpine:3.21.3 apk fetch --recursive nodejs` | placeholder per file until first run | apk files pushed into `rootfs/spike/apks/`, installed **offline** in the guest: `apk add --no-network --no-cache /spike/apks/*.apk` (signatures still verified against the rootfs `/etc/apk/keys`). `npm-*`/`corepack-*`/`yarn-*`/`pnpm-*` file names are rejected at verify AND at push. |
| Next.js 16 template deps (`node_modules`) | npm registry, resolved on the Mac from [`template/package.json`](template/package.json) (`next ^16.0.0`, `react ^19.0.0`); pinned by committing `template/package-lock.json` (npm sha512 integrity entries) after the first fetch | lockfile integrity | `npm ci --ignore-scripts` into staging → tarred with symlinks (`--format gnutar`) → pushed → extracted by device tar into `rootfs/root/hello-next/` |
| `@next/swc-linux-arm64-musl` native binding | `https://registry.npmjs.org/@next/swc-linux-arm64-musl/-/swc-linux-arm64-musl-<version>.tgz`, version forced equal to the resolved `next` version (npm on macOS skips this platform-specific optional dep, so it is fetched explicitly and grafted) | placeholder until first run | unpacked over `node_modules/@next/swc-linux-arm64-musl/` at pack time; all other `@next/swc-*` platform dirs are pruned from the payload |
| Spike template + scripts | this directory (in-repo) | git | pushed as-is |

## Runbook

All commands from the repo root. `SPIKE=docs/mobile-linux/app-runtime-spike`.

### 1. Stage on the Mac (network happens here, and only here)

```sh
SPIKE=docs/mobile-linux/app-runtime-spike

# one-time: PRoot binaries from the pinned fork
clients/android/scripts/build-mobile-linux-native.sh --variant direct

# fetch + verify + pack (first run: expect UNPINNED warnings)
"$SPIKE/scripts/mac/stage-spike.sh" --step all --variant direct --allow-unpinned
```

First-run pinning loop (do this once, then drop `--allow-unpinned` forever):

1. `stage-spike.sh` wrote `"$SPIKE"/build/staging/spike-pins.candidate.json`
   with the real versions + sha256 of the apk closure and swc tarball. Paste
   those values into [`spike-pins.json`](spike-pins.json).
2. Copy `"$SPIKE"/build/staging/app-work/package-lock.json` to
   `"$SPIKE"/template/package-lock.json` and commit it.
3. Re-run `stage-spike.sh --step all --variant direct` (no
   `--allow-unpinned`) — it must now pass verification clean.

### 2. Push to the device (device stays offline)

```sh
"$SPIKE/scripts/mac/push-spike.sh"            # add --serial XXXX with >1 device
```

What it does — the equivalent manual commands, for a fully by-hand run
(`DD=/data/local/tmp/lingxi-appdev-spike`, `P="$SPIKE"/build/staging/payload`):

```sh
DD=/data/local/tmp/lingxi-appdev-spike
P="$SPIKE"/build/staging/payload
adb shell getprop ro.product.cpu.abi                      # must be arm64-v8a
adb shell mkdir -p "$DD/bin" "$DD/proot-tmp"
adb push "$P"/alpine-minirootfs-3.21.3-aarch64.tar.gz "$DD/"
adb shell "sha256sum $DD/alpine-minirootfs-3.21.3-aarch64.tar.gz"   # compare to spike-pins.json
adb shell "mkdir -p $DD/rootfs && cd $DD/rootfs && tar -xzf ../alpine-minirootfs-3.21.3-aarch64.tar.gz"
adb shell "mkdir -p $DD/rootfs/spike" && adb push "$P/apks" "$DD/rootfs/spike/"
adb push "$P/app-payload.tar.gz" "$DD/"
adb shell "mkdir -p $DD/rootfs/root/hello-next && tar -xzf $DD/app-payload.tar.gz -C $DD/rootfs/root/hello-next"
adb shell "mkdir -p $DD/rootfs/opt/spike" && adb push "$P/guest/measure-spike.sh" "$DD/rootfs/opt/spike/"
adb push "$P/bin/proot" "$DD/bin/proot"
adb push "$P/bin/loader" "$DD/bin/loader"
adb push "$P/device/run-in-guest.sh" "$DD/run-in-guest.sh"
adb shell "chmod 755 $DD/bin/proot $DD/bin/loader $DD/run-in-guest.sh"
adb shell "$DD/run-in-guest.sh 'cat /etc/alpine-release'"           # expect 3.21.3
adb shell "$DD/run-in-guest.sh 'apk add --no-network --no-cache /spike/apks/*.apk'"
adb shell "$DD/run-in-guest.sh 'node --version'"                    # expect v22.x (Alpine 3.21)
```

`run-in-guest.sh` wraps the product-shaped PRoot invocation
(`proot -0 --link2symlink -r <rootfs> -b /dev -b /proc -b /sys -w /root`)
with `PROOT_TMP_DIR`, `PROOT_LOADER`, and the PRootKernel guest environment
defaults, plus `NEXT_TELEMETRY_DISABLED=1` so nothing ever dials out.

### 3. Measure

Methodology: airplane mode ON (proves the offline property), device idle and
cool, charger DISCONNECTED for honest battery numbers (reconnect after),
screen may sleep — the run is fully headless. Keep conditions identical
across runs you intend to compare.

```sh
DD=/data/local/tmp/lingxi-appdev-spike
mkdir -p "$SPIKE"/results

# record-only host metrics: before
"$SPIKE/scripts/mac/record-host-metrics.sh" --label before --out "$SPIKE"/results/host-before.json

# the measurement run (~5–25 min depending on device; all progress on stderr)
adb shell "$DD/run-in-guest.sh 'sh /opt/spike/measure-spike.sh'" | tee "$SPIKE"/results/run.stdout.txt

# record-only host metrics: after
"$SPIKE/scripts/mac/record-host-metrics.sh" --label after --out "$SPIKE"/results/host-after.json

# pull the canonical results + per-phase logs
adb shell "cat $DD/rootfs/root/spike-results.json" > "$SPIKE"/results/spike-results.json
adb pull "$DD/rootfs/root/spike-work" "$SPIKE"/results/spike-work
```

`measure-spike.sh` runs, in order:

1. **cold `next dev`** — `.next` deleted, dev server started, readiness =
   first HTTP 200 from `http://127.0.0.1:3000/` whose body contains the
   template marker `SPIKE_PAGE_OK` (i.e. including first page compile —
   the user-meaningful definition, stricter than Next's "Ready in" line)
2. **one HMR edit round-trip** — `sed` rewrites the `SPIKE_HMR_TOKEN_*`
   marker in `app/page.jsx`, then polls until the new token is served
   (file-write → recompile → servable; 0.5 s poll granularity)
3. **warm `next dev` restart** — dev process tree killed, port drained,
   restarted with `.next` caches intact, same readiness definition
4. **`NEXT_OUTPUT=export next build`** — static export; success requires
   exit 0 AND `out/index.html`
5. **disk usage** — `du -sk` of app dir, `node_modules`, `.next`, `out`,
   and the rootfs totals

Peak RSS is sampled once per second across ALL phases by walking
`/proc/<pid>` (shell builtins only, to avoid flooding PRoot's ptrace path
with short-lived children) and summing `VmRSS` over the full descendant tree
of the dev/build node process. Timing uses `/proc/uptime` (monotonic, 10 ms
resolution) because busybox `date` has no sub-second format.

### 4. Read the results

stdout ends with one machine-readable block (also at
`rootfs/root/spike-results.json` in the device staging dir):

```text
-----BEGIN SPIKE RESULTS JSON-----
{ … }
-----END SPIKE RESULTS JSON-----
```

Fields: `environment` (alpine/kernel/node/next versions, cpu count, MemTotal,
**kernel page size** — watch for 16 KB-page devices), `metrics`
(`first_start_s`, `warm_start_s`, `hmr_round_trip_s`, `export_build_s`,
`peak_rss_mb` + per-phase kB peaks, `disk_*_kb`), `record_only` (in-guest
battery %/temp and thermal-zone snapshots before/after), `gates` (below), and
`overall_pass`. A timed-out metric is `null` and fails its gate. Exit code:
`0` all gates pass, `3` gate failure, `2` preflight failure.

## Pass/fail gates

**These four thresholds are copied verbatim from the local-apps V1 acceptance
targets. The spike does not define or tune them** — if the V1 targets change,
update `GATE_*` in `scripts/guest/measure-spike.sh` in the same commit.

| Gate | Threshold | Measured as |
| --- | --- | --- |
| First `next dev` start | <= 120 s | dev-server start → first served page containing `SPIKE_PAGE_OK`, cold `.next` |
| Warm `next dev` restart | <= 30 s | same, with `.next` caches intact |
| Peak RSS, node process tree | <= 800 MB | max of 1 Hz `VmRSS` subtree sums across dev + build phases |
| `next build` (output: export) | <= 180 s | build start → exit 0 with `out/index.html` present |

Record-only (no threshold; reported for the feasibility write-up): HMR
round-trip, disk usage, thermal zones, battery level/temperature (in-guest
sysfs snapshots + `record-host-metrics.sh` dumpsys sidecars), kernel page
size. **overall_pass = all four gates green.** A red gate means phase 0 fails
and the local-apps V1 plan must be revisited before any product integration.

## Troubleshooting

- **`proot` prints nothing / exec format error** — binaries built for the
  wrong ABI or not chmod'ed; re-run `push-spike.sh` (it ELF-checks at pack
  and chmods 755). Remember: only the `shell` domain may exec from
  `/data/local/tmp`; running from an app context is out of scope here.
- **`tar: link … failed` during rootfs extraction** — some /data mounts
  reject hardlinks for non-root users. The pinned minirootfs is expected to
  contain none; if a repack ever introduces them, re-create the archive on
  the Mac with hardlinks dereferenced and record the new digest in
  `spike-pins.json` alongside the original.
- **`apk: unsatisfiable constraints` during offline install** — the pinned
  closure is stale vs the mirror (Alpine ships security bumps in-branch).
  Re-run the Docker resolver, re-pin, re-push. Never "fix" this by giving
  the device network.
- **untrusted signature errors from apk** — key rotation or a corrupted
  download; re-fetch and re-verify on the Mac. Do NOT reach for
  `--allow-untrusted`.
- **dev server never becomes ready** — check `results/spike-work/dev-cold.log`.
  Known suspects to record in the report: `@next/swc` binding failing to load
  (musl/page-size mismatch — check `environment.kernel_page_size_kb`),
  inotify watch exhaustion (`fs.inotify.max_user_watches` is low on some
  ROMs and not settable without root), OOM kills (check `dmesg` via adb).
- **wrong Next major staged** — verify fails unless the resolved version is
  16.x; re-pin `template/package.json` if npm resolved something newer.

## Cleanup

```sh
adb shell rm -rf /data/local/tmp/lingxi-appdev-spike
rm -rf "$SPIKE"/build
```

## Files

| Path | Purpose |
| --- | --- |
| `spike-pins.json` | pin manifest for every external artifact |
| `template/` | minimal Next.js 16 app-router template (markers are load-bearing) |
| `scripts/mac/stage-spike.sh` | fetch / verify / pack on the Mac |
| `scripts/mac/push-spike.sh` | adb push + offline on-device install |
| `scripts/mac/record-host-metrics.sh` | record-only battery/thermal via dumpsys |
| `scripts/device/run-in-guest.sh` | PRoot wrapper on the device |
| `scripts/guest/measure-spike.sh` | the measurements + gates, inside the guest |
| `build/`, `results/` | gitignored staging + run outputs |
