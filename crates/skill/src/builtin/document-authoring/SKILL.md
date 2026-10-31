---
name: document-authoring
description: Create, edit or export documents in LingXi using workspace files or an available document service. Use for saved deliverables; LingXi product/API questions use lingxi-api.
---

# Document authoring in LingXi

Create the document the user requested, using their language, format and chosen
location. An answer, comparison, summary or plan requested in chat stays in chat
unless the user also asks to save it as a document. A document request does not
require creating an empty shell before reading the user's sources.

Use `lingxi-api` for LingXi capability, configuration or API questions. When the
requested document is about LingXi, use that guide for product facts and this
skill for authoring; keep product knowledge in the guide's references.

## Choose an available destination

- For an existing file, read it and edit it in place, respecting repository
  instructions, its format and nearby conventions. Repository documentation
  belongs in the repository's existing documentation structure.
- For a named document service or an existing service link, inspect the tools
  actually available for that service and follow their supported read/edit
  workflow. Keep the existing document identity when updating. Do not assume a
  Claude docs connector, a `guide` method, or a particular API shape exists.
- For a new local document without a required format, use Markdown in the
  selected workspace with a meaningful filename. Use the host's actual file
  tools and returned paths; mobile guest paths and remote host paths are not
  interchangeable with Desktop paths.

Use a connected service only when its tools are available and it fits the
requested destination. If a named destination cannot be accessed, explain the
missing capability and ask before substituting a different destination. Do not
claim a cloud document exists merely because a local file was written.

## Write and verify the result

Read the supplied material and relevant existing content before drafting.
Preserve factual claims, citations and the user's requested audience and scope.
Ask only for missing information that changes the result; do not require a
fixed interview, layout or document structure for every request.

For PDF, Word, slides or spreadsheets requested explicitly, use a suitable
available writer, library or format-specific skill. A Markdown file with a
different extension is not an export. Verify the actual output with an
appropriate reader or preview; inspect layout when formatting matters.

After a file edit, read back the changed content. For a service document, use its
returned identity and read-back/version facilities when supported. Distinguish
saved content, generated exports and sharing status. Return a reachable file
reference or the real service link, with the important verification limits.

Creating a document does not by itself authorize public Artifact publication,
changing sharing permissions, uploading to another service or sending messages.
Use those actions only when the user's request includes them. A Visualization
or Local App is appropriate when the user requests an interactive result;
ordinary documents do not require either host capability.
