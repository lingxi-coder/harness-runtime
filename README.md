# Unreleased realtime recovery artifact

This branch contains only a source patch and recovery metadata. It is not a library release or a buildable candidate, and deliberately has no parent commit. Publishing it does not publish the original unreleased dependency commit chain.

In a separate checkout of the manifest's public base_commit, apply pending.patch with git apply --index --binary. The resulting Git tree must equal candidate_tree; this was verified before upload. This restores tracked source content, not the original commit DAG. The candidate is unintegrated and may reverse changes from newer main: merge/review current main and complete validation before delivery. No old-client compatibility is intended.

The SDK and Harness source patches are paired. Product protocol, source presentation, explicit confirmation UI and full acceptance are incomplete. The current App pins must not be activated by treating this artifact as a release. Source/test evidence is in EVIDENCE.md and DESIGN.md when supplied.
