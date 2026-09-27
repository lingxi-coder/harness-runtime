# Node.js sourcing memo — MobileLinux app runtime

- Date: 2026-07-30 (all package/registry facts below were fetched live on this date)
- Scope: how to source a Node.js runtime for the Android MobileLinux rootfs
  (Alpine 3.21.3 minirootfs, branch `v3.21`, musl libc, ABIs `arm64-v8a`
  [aarch64] and `x86_64`; see `docs/mobile-linux/rootfs/README.md` and
  `docs/mobile-linux/mobile-linux-pins.json`).
- Trigger: the draft app-runtime plan pinned "Node.js 24.18.0". Official
  nodejs.org binaries are glibc-only, so that pin cannot be satisfied on the
  musl rootfs from official channels. This memo resolves what we can actually
  ship.

## TL;DR

Ship the Alpine-packaged Node (option a). For V1 on the existing `v3.21`
rootfs pin that is `nodejs` **22.23.0-r0** (main repo, identical for aarch64
and x86_64), which satisfies Next.js 16.2.x (`engines.node: ">=20.9.0"`).
Drop the "Node.js 24.18.0" pin: it is already superseded upstream by 24.18.1,
and the only prebuilt musl-arm64 channel for Node 24 (nodejs/unofficial-builds)
is explicitly experimental and did not publish an arm64-musl artifact for
24.18.1 as of 2026-07-30. Plan a whole-rootfs bump to Alpine 3.24.x (which
carries `nodejs` 24.18.x in main) before the `v3.21` branch leaves security
support on **2026-11-01**.

## 1. Alpine package versions (checked 2026-07-30 on pkgs.alpinelinux.org)

Newest stable Alpine branch as of 2026-07-30 is **v3.24** (release 3.24.1,
2026-06-13). The rootfs pin is on **v3.21** (branch latest patch: 3.21.7,
2026-04-15; the pinned archive 3.21.3 is four patch releases behind its own
branch).

| Branch | Package | aarch64 | x86_64 | Repo | Notes |
|---|---|---|---|---|---|
| v3.21 | `nodejs` | 22.23.0-r0 | 22.23.0-r0 | main | armv7/armhf/x86/ppc64le already at 22.23.2-r0 — upstream 22.23.2 landed 2026-07-29 and Alpine packaged it within a day; the aarch64/x86_64 builders lag by days, not weeks |
| v3.21 | `nodejs-current` | 23.2.0-r1 | 23.2.0-r1 | community | Frozen at a 2024-11-18 build. Node 23 is an odd-numbered non-LTS line that left upstream support in mid-2025 — unusable |
| v3.21 | `npm` | 10.9.1-r0 | (same line) | community | npm is packaged separately from `nodejs` in Alpine |
| v3.24 | `nodejs` | 24.18.1-r0 | 24.17.0-r0 | main | Transient cross-arch skew: the x86_64 build dates 2026-06-22; aarch64 already has the 2026-07-29 upstream security patch 24.18.1. Expect convergence, but per-ABI pins must tolerate skew windows |
| v3.24 | `nodejs-current` | 26.3.1-r0 | 26.3.1-r0 | community | Built 2026-07-17; Node 26 is the next even line (upstream latest 26.5.1) |
| v3.24 | `npm` | 11.12.1-r0 | (same line) | community | |

Support windows (endoflife.date, checked 2026-07-30):

- Alpine 3.21: EOL **2026-11-01** — under four months away. This bounds any
  V1 that stays on the current rootfs pin.
- Alpine 3.24: EOL 2028-06-01.
- Node 22 (LTS "Jod"): latest patch 22.23.2 (2026-07-29); upstream EOL
  **2027-04-30**.
- Node 24 (current LTS line, entered LTS October 2025 per the Node.js release
  schedule): latest patch 24.18.1 (2026-07-29); upstream EOL **2028-04-30**.

So: "Node.js 24.18.0" is not the current 24.x — **24.18.1** is (a 2026-07-29
security-cadence release). Any pin of 24.18.0 would ship known-superseded
bits on day one.

## 2. Prebuilt musl-arm64 Node 24.x (nodejs/unofficial-builds)

Official nodejs.org `latest-v24.x` (= v24.18.1) ships Linux tarballs for
x64/arm64/ppc64le/s390x — **all glibc, no musl variants** (verified in the
directory listing).

The nodejs/unofficial-builds project (unofficial-builds.nodejs.org) does list
`linux-arm64-musl` in its platform matrix, alongside `linux-x64-musl`. Actual
release coverage, from its `download/release/index.json` and per-release
directories (checked 2026-07-30):

- v24.0.0 – v24.8.0: **no** arm64-musl artifacts.
- v24.9.0 – v24.18.0: arm64-musl artifacts present (e.g.
  `node-v24.18.0-linux-arm64-musl.tar.xz` exists, with SHASUMS256.txt).
- **v24.18.1 (the current 24.x security release): no arm64-musl artifact** —
  only `linux-x64-musl` was published as of 2026-07-30.
- Newer lines are equally spotty (v26.5.0 no, v26.5.1 yes).

Trustworthiness, quoting their README: "This project is experimental: its
output is not guaranteed to remain consistent and its existence is not
guaranteed into the future." Builds have "minimal or no testing"; the user
community is "expected to assist in their maintenance."

Conclusion: unofficial-builds is not a shippable supply channel for a
security-sensitive, pinned mobile rootfs — precisely at security-release time
(24.18.1) the arm64-musl artifact is missing, which would leave us choosing
between shipping a known-CVE build and blocking a release on a best-effort
third party.

## 3. Next.js 16.2.x requirements (npm registry, checked 2026-07-30)

- Current `latest` dist-tag of `next` is **16.2.12**.
- `next@16.2.12` declares `engines: { "node": ">=20.9.0" }`.
- Its optionalDependencies include the full SWC binary set at 16.2.12,
  including **`@next/swc-linux-arm64-musl`** and `@next/swc-linux-x64-musl`.
- `@next/swc-linux-arm64-musl@16.2.12` exists on the registry with
  `os: linux`, `cpu: arm64`, `libc: musl`.

Implications:

- Node 22.23.x (Alpine v3.21) and Node 24.18.x (Alpine v3.24) both satisfy
  Next.js 16.2.x. Nothing about Next.js forces Node 24.
- Next.js's native compiler path is first-class on musl arm64 — no glibc
  requirement comes from the framework side.

## 4. Options matrix and recommendation

| Option | What ships | Pros | Cons |
|---|---|---|---|
| (a) Vendor Alpine apk Node | v3.21: `nodejs` 22.23.0-r0 (+ `npm` 10.9.1-r0); after a rootfs bump to v3.24: `nodejs` 24.18.x-r0 (+ `npm` 11.12.1-r0) | Built by the distro against the exact musl/toolchain in the rootfs; security patches arrive in-branch within ~days of upstream (observed: upstream 22.23.2 on 2026-07-29 → apks on 2026-07-30); slots directly into the existing pin/SBOM/manifest evidence pipeline (`rootfs_tool.py`, SPDX, executable allowlist); zero new build infrastructure | v3.21 gives Node 22, not the drafted Node 24; the v3.21 patch flow stops at branch EOL 2026-11-01; transient per-arch builder skew means per-ABI pinned versions can differ for days |
| (b) Self-build musl-arm64 Node 24.18.x | Our own cross-compiled tarball | Exact version control; no third-party trust beyond source | We become a Node distributor: musl cross toolchain + the musl patch set Alpine already maintains; multi-hour V8 builds per arch per release; must rebuild-and-repin within days of every upstream security release (both 22.x and 24.x shipped patches on 2026-07-29 alone) or ship known CVEs. Rough cost: 1–2 engineer-weeks initial + recurring per-release effort and CI capacity (estimate, not measured). Duplicates Alpine's existing, faster pipeline |
| (c) Migrate rootfs to glibc | Official nodejs.org `linux-arm64` / `linux-x64` binaries | Fully supported official binaries; unblocks any future glibc-only tooling | Invalidates the entire pinned Alpine evidence contract (mobile-linux-pins.json digests, minirootfs archives, SPDX/manifest/allowlist tooling, apk policy) and restarts the rootfs workstream; larger base image; a distro + security-update mechanism must be re-chosen and re-audited. Framework side gives no motivation: Next.js 16.2.x is fully supported on musl (section 3) |

### Recommendation for V1: option (a), vendor the Alpine apk Node

1. **Now (V1 on the existing pin):** add `nodejs` 22.23.0-r0 (and `npm`
   10.9.1-r0 if npm is needed at runtime) from Alpine v3.21 main/community to
   the vendored package set, staged and hashed through the existing rootfs
   evidence pipeline. This satisfies Next.js 16.2.x today with zero new
   infrastructure and keeps runtime, libc, and toolchain from a single
   coherent distro branch. (Extending the "fixed primary package set" in
   `docs/mobile-linux/rootfs/README.md` is a rootfs-workstream change; this
   memo deliberately does not edit that file.)
2. **Retire the "Node.js 24.18.0" pin.** It is superseded upstream (24.18.1),
   has no official musl build, and its only prebuilt musl-arm64 source is an
   explicitly experimental channel that skipped the current security release
   on arm64-musl. If the plan needs the Node 24 line, the supported route is
   the Alpine v3.24 `nodejs` package (24.18.1-r0 on aarch64 today).
3. **Before 2026-11-01:** bump the whole rootfs baseline from Alpine 3.21.3 to
   3.24.x and move Node to 24.18.x-r0 from v3.24 main in the same change.
   This lands the drafted Node 24 major with distro support to 2028 on both
   the OS (2028-06-01) and Node LTS (2028-04-30) axes. When re-pinning, check
   per-ABI apk versions individually — on 2026-07-30 v3.24 aarch64 was at
   24.18.1-r0 while x86_64 was still at 24.17.0-r0.

### Security-patch cadence implications (applies to whichever option wins)

- Because the rootfs is pinned by digest, no option gives automatic updates:
  every Node security release requires a deliberate re-pin + re-stage +
  re-release of the evidence set. Option (a) minimizes the latency floor
  (Alpine repackages within ~days) and the work per event (bump one apk pin);
  option (b) makes us the bottleneck; option (c) ties cadence to nodejs.org
  plus a full rootfs re-verification.
- Define a monitoring trigger now: Node.js security release announcements and
  Alpine `secfixes` for the pinned branch. Upstream shipped patches for both
  active lines (22.23.2, 24.18.1) on 2026-07-29 — this is a
  roughly-every-4-to-8-weeks event, not a rare one.
- Hard deadline created by staying on v3.21 for V1: the branch's security
  support ends **2026-11-01**. After that date the pinned rootfs (already at
  3.21.3 vs branch-latest 3.21.7) and its Node package receive no fixes, so
  the 3.24 migration in step 3 is a scheduled commitment, not a nice-to-have.

## Data-quality caveats

- Package pages were read via a summarizing fetch tool; the two anomalies it
  reported (v3.21 aarch64/x86_64 at 22.23.0-r0 while sibling arches are at
  22.23.2-r0; v3.24 x86_64 at 24.17.0-r0 vs aarch64 24.18.1-r0) are
  consistent with normal Alpine builder lag and were cross-checked with a
  second arch-filtered query for the v3.24 x86_64 case (build date
  2026-06-22). Re-verify exact `-rN` strings at pin time; they move with the
  branches.
- endoflife.date's Node 24 row rendered ambiguously through the fetch tool;
  the EOL dates quoted above (22 → 2027-04-30, 24 → 2028-04-30) were reported
  cleanly, and Node 24's LTS-entry month (October 2025) is from the Node.js
  release schedule.

## Sources (all fetched 2026-07-30)

- https://pkgs.alpinelinux.org/packages?name=nodejs&branch=v3.21 (and
  `nodejs-current`, `npm`; branch v3.24 variants; arch-filtered queries for
  aarch64/x86_64)
- https://nodejs.org/dist/latest-v24.x/ (v24.18.1; glibc-only Linux tarballs)
- https://github.com/nodejs/unofficial-builds (platform matrix + experimental
  disclaimer)
- https://unofficial-builds.nodejs.org/download/release/index.json and the
  v24.18.0 / v24.18.1 release directories (arm64-musl coverage)
- https://registry.npmjs.org/next/16.2.12 and
  https://registry.npmjs.org/-/package/next/dist-tags (engines, swc
  optionalDependencies, latest = 16.2.12)
- https://registry.npmjs.org/@next%2Fswc-linux-arm64-musl/16.2.12 and its
  dist-tags (musl arm64 binary exists for 16.2.12)
- https://endoflife.date/alpine-linux and https://endoflife.date/nodejs
  (branch EOL dates, latest patches)
- https://www.alpinelinux.org/posts/Alpine-3.20.10-3.21.7-3.22.4-3.23.4-released.html
  (branch patch state) and Alpine downloads page (3.24.1 current)
