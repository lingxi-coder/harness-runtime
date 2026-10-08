function caseFromUrl(url) {
  const marker = "/fixture/";
  const index = typeof url === "string" ? url.lastIndexOf(marker) : -1;
  return index < 0 ? "unknown-case" : url.slice(index + marker.length);
}

function artifactFor(caseId) {
  if (caseId === "artifact-read-null") {
    return null;
  }
  if (caseId === "bytes-missing-slug-number-seeded-true") {
    return { slug: 13, ver: "v1", seeded: true };
  }
  if (caseId === "slug-missing") {
    return { ver: "v1", seeded: false };
  }
  if (caseId === "slug-wrong-type") {
    return { slug: 13, ver: "v1", seeded: false };
  }
  if (caseId === "seeded-true") {
    return { slug: "fixture-artifact", ver: "v1", seeded: true };
  }
  if (caseId === "seeded-string") {
    return { slug: "fixture-artifact", ver: "v1", seeded: "false" };
  }
  return {
    slug: "fixture-artifact",
    ver: "v1",
    seeded: false,
    futureArtifactField: "strip-this",
    nestedUnknown: { future: "strip-this-too" },
  };
}

export function register(on) {
  on("tool.call", { tool: "WebFetch" }, async (_$, event, next) => {
    const nativeResult = await next(event);
    const caseId = caseFromUrl(event.url);
    const result = {
      bytes: 24,
      code: 200,
      codeText: "200 OK",
      result: "fixed loopback WebFetch result",
      durationMs: 1,
      url: event.url,
      artifactRead: artifactFor(caseId),
    };
    if (caseId === "bytes-missing-slug-number-seeded-true") {
      delete result.bytes;
    }
    if (caseId === "valid-unknown-keys") {
      result.unknownTopLevel = { ignored: true };
    }
    const replacement = { ...nativeResult, result, text: result.result };
    console.log("NATIVE_WEBFETCH_OUTPUT=" + JSON.stringify({
      caseId,
      tool: event.tool,
      tool_use_id: event.tool_use_id,
      eventKeys: Object.keys(event),
      url: event.url,
      prompt: event.prompt,
      nativeResult,
      result,
      replacement,
    }));
    return replacement;
  });
}
