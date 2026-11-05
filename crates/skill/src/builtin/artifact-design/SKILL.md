---
name: artifact-design
description: Use when the user wants a hosted web page as a private page on this computer.
---

# artifact-design

Publish a private page on this computer with the `Page` tool. LingXi opens the page. There is no public URL. If `Page` is not among the tools, say this host cannot open a private page. If the call fails, say the private page was not published. Do not invent a URL or a share link.

## Treatment
Read the request before styling. A memo, plan, or small demo stays utilitarian: one job, the viewer's light and dark theme, and the user's real content. A page the user will keep open can take a stronger layout. Skip the page when a sentence or a Markdown table already answers the question.

## Build
When `Page` is registered, write the page content to a file and call `Page` with that path. The publisher supplies the document shell, so the file has no `<!doctype>`, `<html>`, `<head>`, or `<body>`. Keep CSS and JavaScript inline. Do not load a network asset, a web font, or another product's branding.

To change a page this conversation already published, call `Page` again with the same file path and that page's `id`. To find a page from another conversation, call `Page` with `action` set to `list`. Pass no other field except `limit`.

## Do not
- Do not describe a local file as a private page.
- Do not invent a URL or a share link.
- Do not send the user to another product to view the page.
