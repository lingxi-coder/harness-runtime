---
name: mobile-device
description: Use for direct actions and status checks on the user's Android or iOS device.
session-modes: [chat, code]
---

# Mobile device

Use the available native tools below to fulfill a device request directly. No manifest, app identifier, JavaScript bridge, or generated application is needed. The available tool schema is authoritative for arguments and current support.

Call only the operations needed for the user's request. Read returned data before reporting completion. If the OS denies permission, the user cancels, or a service is unavailable, report that result and stop the affected operation; do not retry through a different device interface.

## device_status

Call `device_status` with `{}` to read `battery_percent`, `charging`, `network`, and `low_power_mode`. All except `network` may be null; do not turn unknown values into zero or false. The snapshot describes device state, not a guarantee that an external service is reachable.

## clipboard

Call `clipboard` with `{"action":"get"}` only when clipboard contents are needed for the request. The result's `text` can be null. To copy the requested text, call `{"action":"set","text":"..."}` and check `set`. Do not overwrite the clipboard merely to stage an unrelated action.

## share

Call `share` with `{"text":"..."}`, `{"url":"https://..."}`, or both, to present the native share sheet with the requested content. Check `shared`; a false result is not successful sharing. Even a successful handoff is not evidence of delivery to a particular person or service.

## notification

Call `notification` with `{"action":"post","title":"...","body":"..."}` and an optional `tag` to post a local notification now. Check `posted`. This operation does not schedule a future reminder; posting success does not prove that the user saw the notification. Keep sensitive details out of notification previews unless the user requested them.

## haptics

Call `haptics` with `{"style":"light"}` for one bounded feedback event. Supported styles are `light`, `medium`, `heavy`, `success`, `warning`, and `error`. Choose the requested style or a light tap when none is specified; do not loop feedback events. Check `triggered` and the returned `style`.

## open_url

Call `open_url` with `{"url":"https://..."}` to open a requested destination. Only `http`, `https`, `mailto`, and `tel` schemes are supported, with at most 4096 characters. Use the exact intended destination and inspect `requested` and the returned canonical `url`. A true `requested` means the host submitted the URL to the operating system; it does not prove that a message was sent, a purchase completed, a payment submitted, or any operation in that app succeeded.
