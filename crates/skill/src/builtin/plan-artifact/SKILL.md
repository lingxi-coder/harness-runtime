---
name: plan-artifact
description: Use when the user asks to publish a plan, RFC, or design doc as a hosted page.
---

# plan-artifact

A private plan page follows `artifact-design`. Publish a private page on this computer with the `Page` tool. LingXi opens the page. There is no public URL. If `Page` is not among the tools, say this host cannot open a private page. If the call fails, say the private page was not published. Do not invent a URL or a share link.

Approval of an implementation plan still uses `ExitPlanMode`. This skill does not replace that. There is no plan-share command.

## Build
Keep the plan's real sections and order. Drop a section that has no content instead of inventing one.

When `Page` is registered, write the plan as page content, without a document shell, and call `Page` with that file path. To revise a page this conversation published, call `Page` again with the same path and that page's `id`. To find an older page, call `Page` with `action` set to `list`. Pass no other field except `limit`.
