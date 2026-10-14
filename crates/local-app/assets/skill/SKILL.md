---
name: local-app
description: Build a small app the user keeps on this device, such as a tracker, calculator, timer, checklist or game, as one HTML file that opens from the chat and the Apps page.
session-modes: chat, code
---

# Local apps

Build the app as ONE self-contained HTML document and publish it with the `LocalApp` tool. It joins the user's app library on this device: it opens full screen from the card under your tool call and from the Apps page, works offline, and keeps its own data between launches.

## When to use it

- The user wants an app, tool, tracker, game, calculator, timer, checklist or similar that they will open again.
- For a one-off chart or explainer inside the conversation, use an inline visualization instead when one is available.

## Workflow

1. If the request is vague, ask at most three short questions with `AskUserQuestion`: what it is for, the main screens, and what it must remember. Otherwise build it right away with sensible defaults.
2. Write the complete document: `<!doctype html>`, `<html lang="…">`, a `<head>` with `<meta charset="utf-8">`, `<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">` and a `<title>`, then the body. Put all CSS in `<style>` and all JavaScript in classic `<script>` tags; draw icons as inline SVG and embed images as `data:` URIs.
3. Publish it: call `LocalApp` with a short `name` (at most 60 characters), a one-line `summary`, and the document in `html`.
<!-- if-shell -->
   For a long script, Write the document to an `.html` file, check its script with the `{{SHELL_TOOL}}` tool when an interpreter such as `node` is available, then publish with `file_path` instead of `html`.
<!-- end-if-shell -->
4. Fix whatever the tool reports and publish again. It rejects anything that is not a complete HTML document and warns about resources that would break offline.
5. Tell the user what the app does and that it opens from the card or the Apps page. Do not paste the HTML into your reply.

## Changing an app

- In the same conversation, publish again with the app's `id`. Each publish is a new revision; the app keeps its stored data.
- For an app made earlier, call `LocalApp` with `action: "list"` to find its `id`, then `action: "read"` with that `id` to get the current document. Change it and publish with the same `id`.
- Keep stored data readable by the new version: keep the keys, or store a schema version next to the data and convert old data when the app starts.

## Rules

- **Offline.** No CDN scripts, web fonts, remote images or other network resources. Inline everything. Use plain JavaScript and CSS: no frameworks, packages or build step.
- **Data.** Keep small values in `localStorage` and records in IndexedDB. Each app has its own storage, which is deleted with the app. Validate what you read back, show the user when a save fails, and offer export and import as a JSON file for anything they would hate to lose.
- **Phone first.** The layout works from 320 px wide up. Respect the safe areas with `env(safe-area-inset-*)`, make touch targets at least 44 px, avoid hover-only controls, and use `100dvh` rather than `100vh`.
- **Look.** Follow `prefers-color-scheme` for light and dark, use the system font stack and one accent color.
- **Accessibility.** Label every control, give icon-only buttons an `aria-label`, and keep text contrast readable.
- **What a page cannot do.** No background work or notifications while the app is closed; no camera, microphone or location (they are denied); no calendar or contacts (offer an `.ics` or `.vcf` download instead); no AI model calls; no Web Workers or service workers. Links to other sites open outside the app after the user confirms.

## Bigger apps

When the user wants a framework, a game engine, 3D graphics or a build step, and the `local-app-builder` plugin is installed, use its `create-local-app` skill instead. It builds a single-file app on a computer and publishes it through this same tool.
