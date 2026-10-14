// Only the schema/library boundary is adapted. Classifier functions in the
// adjacent native asset retain their shipped 2.1.296 source bytes.
const Y = JSON.parse;
const Lt = (text, needle) => {
  if (needle === "") return text.length + 1;
  let count = 0, at = 0;
  while ((at = text.indexOf(needle, at)) !== -1) { count++; at += needle.length; }
  return count;
};
const F = (items, predicate) => items.reduce((count, item) => count + Number(predicate(item)), 0);
function D() {
  return { safeParse(value) {
    if (value === null || typeof value !== "object" || Array.isArray(value)) return {success:false};
    const data = {};
    for (const key of ["state", "detail", "tempo", "needs"]) {
      const member = value[key];
      if (member !== undefined && member !== null && typeof member !== "string") return {success:false};
      if (key in value) data[key] = member;
    }
    const output = value.output;
    if (output !== undefined && output !== null && (typeof output !== "object" || Array.isArray(output))) return {success:false};
    if ("output" in value) data.output = output;
    return {success:true, data};
  }};
}
