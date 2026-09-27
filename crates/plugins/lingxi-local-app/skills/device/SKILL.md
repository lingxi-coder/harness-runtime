---
name: device
description: Guide a local app's own JS source in reading window.lingxi.v2's live device context and capability bridges, and explain the declare-then-prompt permission UX each one goes through.
---

# Use a local app's device context and capabilities

Own guiding correct use of `window.lingxi.v2` from *inside an app's own
page code* — its live device context, its capability bridges (camera,
microphone, location, notifications, clipboard, share, speech, files,
calendar, contacts, network, LLM side-calls, Agent sessions), and the
permission UX each one goes through. This skill explains and reviews that
JS surface; it never calls Swift, Compose, or Capacitor itself — those are
reached only *through* this same bridge, by the client, not by this skill.

## The bridge object

`window.lingxi.v2` is injected into the app's own WebView page by the
native host (idempotently — the injection script itself checks
`if (window.lingxi?.v2) return;` before running again). The shipped runtime
library (`lib/lingxi-bridge.js` in a scaffolded app) wraps every call behind
a `getLingXiBridge()` helper that returns `null` before the object exists;
prefer that helper (or the same null-check) over assuming the global is
already there on first paint.

## Device context — live, and NOT the same thing as the manifest's

Two different "device context" objects exist, and they must not be
conflated:

- **`window.lingxi.v2.deviceContext`** (same object as
  `window.lingxi.v2.runtime.deviceContext`) is what the page actually reads.
  `os` and `formFactor` are fixed once, at injection, by the native host —
  iOS gives `os:"ios"`, `formFactor:"iphone"|"ipad"`; Android gives
  `os:"android"`, `formFactor:"phone"|"tablet"`. Never guess or hardcode
  either value; they vary by platform and the app must read them, not infer
  them. `viewport:{width,height}`, `safeArea:{top,right,bottom,left}`,
  `colorScheme` (`"dark"|"light"`), `reducedMotion` (boolean), and
  `inputMode` (`"pointer"|"touch"`) are **live getters**, recomputed from
  the browser's own state (`visualViewport`, `env(safe-area-inset-*)`,
  `prefers-color-scheme`, `prefers-reduced-motion`, `pointer: fine`) on
  every read — treat them as values that can change mid-session (rotation,
  split-screen, a system dark-mode toggle), and never cache a value read
  once.
- **The manifest's own `device_context`** — the thing `LocalAppManifest`'s
  description says is "host-derived and recorded automatically; never
  declare it" — is a much smaller, *persisted* pair: only `{os,
  formFactor}`, stamped once at generation time. It does not carry
  `viewport`, `safeArea`, `colorScheme`, `reducedMotion`, or `inputMode` —
  those exist only as the live bridge object above, never in the manifest.

## The permission ladder every capability bridge runs

Every `device.*`, `clipboard.*`, `files.*`, `calendar.*`, `contacts.*`,
`share`, and `synthesizeSpeech` call runs the same host-side ladder, in
order: (1) the app's manifest must already declare the matching
capability, or the call fails immediately as "capability not declared,"
with no prompt at all; (2) the ordinary persisted → session → prompt ladder
runs, showing the user a first-use reason in their own language; (3) only
then does it dispatch to the native handler and come back as a JSON
envelope — a media result comes back **base64**, which the shipped
`mediaObjectURL()` helper turns into a `Blob` URL; never treat the base64
string itself as something to display directly.

`deviceContext` and `runtime.info`/`runtime.status` need no capability
declaration at all — they're always available. `device.status` is the one
exception among device calls: it only checks the manifest declared
`device_status` and raises no separate user prompt, unlike every other
`device.*` call.

| JS call | Declared capability | First-use prompt (verbatim) |
|---|---|---|
| `device.capturePhoto` | `camera` | "应用请求使用相机拍摄一张照片。" |
| `device.pickImage` | `photo_library` | "应用请求从相册选择一张图片。" |
| `device.recordAudioStart`/`recordAudioStop` | `microphone` | "应用请求使用麦克风录音。" |
| `device.transcribeSpeech` | `microphone` (same grant as recording — there is no separate speech capability) | "应用请求使用麦克风把你说的话转写成文字。" |
| `device.getLocation` | `location` | "应用请求获取一次当前位置。" |
| `device.postNotification` | `notifications` | "应用请求发送本地通知。" |
| `clipboard.getText`/`setText` | `clipboard` | "应用请求读取或写入系统剪贴板。" |
| `device.share` | `share` | "应用请求打开系统分享面板。" |
| `device.synthesizeSpeech` | `text_to_speech` | "应用请求将文字转换为语音。" |
| `device.haptics` | `haptics` | "应用请求触发一次短促的触觉反馈。" |
| `device.deepLink` | `deep_link` | "应用请求打开一个外部链接。" |
| `calendar.listEvents` | `calendar` | "应用请求读取你指定时间范围内的日历事件。" |
| `contacts.search` | `contacts` | "应用请求搜索你的联系人信息。" |
| `device.status` | `device_status` | *(declared-only; no prompt)* |

## Cancellation isn't uniform — read the shape, not just success/failure

A user cancelling a native picker is **not** always an error. `device.share`
resolves normally with `{shared:false, cancelled:true}` on cancel. Camera
and photo-library calls instead **reject** with an error carrying code
`"cancelled"`. Guide app code to branch on the actual shape each call
returns rather than assuming every cancel path looks the same.

## A few limits worth knowing before debugging a rejected call

- `device.recordAudioStart`'s `maxDurationMs` clamps to 1,000–300,000ms
  (default 120,000ms if omitted).
- `device.postNotification` needs `title` (1-100 chars) and `body`
  (1-500 chars); an optional `tag` must match `^[a-z0-9][a-z0-9_-]{0,63}$` —
  the host prefixes it internally so one app can never replace another
  app's (or the assistant's) notification, but echoes back only the
  page's own tag.
- `device.share` needs at least one of `text` (≤20,000 chars), `url`
  (≤4,096 chars), or an image `mediaId` from a prior capture/pick.
- `device.deepLink`'s `url` (≤4,096 chars) must parse, must carry no
  embedded username/password, and an `http`/`https` link must have a host.
- Bridge request payloads are capped client-side before they're even sent:
  65,536 bytes for ordinary control calls, 4 MiB for `files.read`/`write`,
  8 MiB for `llm.chat`/`llm.stream`.
- External network access is **only** `window.lingxi.v2.network.fetch` (a
  raw `fetch`/`XHR` to a different origin throws a `TypeError` in the
  injected page). The target must be plain HTTPS, carry no embedded
  credentials, resolve to a public (non-private, non-loopback) address, and
  its domain must already be in the manifest's `allowed_domains` and
  durably approved.

## Boundaries

- Never call a native API directly, and never invent a bridge method that
  isn't in the table above — everything a page can reach goes through
  `window.lingxi.v2`, full stop.
- Never suggest polyfilling a capability with a browser API instead (e.g. a
  raw `navigator.geolocation` call) — that bypasses the manifest
  declaration and the permission prompt entirely and will not work inside
  the WebView sandbox this app runs in.
- Declaring a NEW capability in the manifest is `LocalAppManifest`'s job,
  not this skill's — this skill only guides using capabilities that are
  already declared, and explains a `capability_not_declared` failure back
  to whoever's asking, rather than working around it.
- Reading or writing the app's own collections goes through
  `$local-app-data`; scheduling a background flow that itself calls into
  this bridge goes through `$local-app-background` — this skill is the
  bridge surface itself, not the host tools that operate on an app from
  outside it.
