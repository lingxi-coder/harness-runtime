---
name: local-app-capture-view
description: Capture a still frame or before/after frames of a local app's view — the only evidence a canvas/WebGL surface can give. Never claims an interaction succeeded from a picture alone.
---

# Capture a local app's view

Own visual evidence about a running app: what its own view actually renders,
as an image. This is the only evidence available for a canvas or WebGL
surface, which has no elements for a DOM snapshot to describe.

## The tool

Call `LocalAppCaptureUi` with `app_id` and an optional `rect:{x, y, width,
height}` in CSS pixels to crop; omit `rect` to capture the whole view. The
result is an actual image plus a JSON sidecar:

- `image_width`, `image_height` — the frame's own pixel size. This is
  required to convert anything you see in the picture back into the CSS
  pixels `$local-app-interact`'s pointer action takes; never assume the
  image is 1:1 with `viewport`.
- `viewport` — `{width, height}` in CSS pixels, always the whole view
  regardless of whether `rect` cropped the image.
- `device_pixel_ratio`, `jpeg_quality`.
- `capture_rect:{x, y, width, height}` — present only when `rect` was
  requested, and is the actual clamped region captured, not necessarily the
  one you asked for.

## Converting an image coordinate back to CSS pixels

- Whole-view capture (no `capture_rect` in the result): `CSS_x = ix *
  viewport.width / image_width`, and the same for `y`/`height`. This only
  holds because a whole-view capture and `viewport` share the same origin.
- Cropped capture (`capture_rect` present): `viewport` does **not** apply —
  it has the wrong scale and no offset for a crop. Use `CSS_x =
  capture_rect.x + ix * capture_rect.width / image_width`, and the same for
  `y`/`height`.

## Motion evidence

One frame only proves the surface rendered once at that moment. To support
any claim that something is animating, playing, or changed as a result of an
action, capture at least two frames — before and after, or a beat apart —
and compare them; do not claim motion from a single frame.

## When the app view isn't there

If the tool reports the frame came back empty or that there was no frame to
capture, the app's view is not currently on screen (or produced nothing) —
report that plainly rather than retrying blindly or treating a failed
capture as a blank-but-successful one.

## Boundaries

- This captures a frame, not a result. A screenshot that looks right after a
  tap is not proof the tap was received or handled — pair it with
  `$local-app-interact`'s own action result, or `$local-app-inspect-view`'s
  structural state, before saying an interaction "worked." On its own, this
  skill reports only what the pixels show.
- Do not drive pointer or keyboard input from here — that is
  `$local-app-interact`'s job; this tool only captures.
- Do not judge pass/fail or diagnose a root cause here; hand the frames to
  `$local-app-test` or `$local-app-debug` for that.
