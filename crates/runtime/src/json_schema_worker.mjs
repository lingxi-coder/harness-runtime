import { createInterface } from "node:readline";
import { Ajv, Ajv2020, addFormats } from "./ajv-8.18.0-profiles.bundle.mjs";

if (typeof Bun === "undefined") throw new Error("Bun is required for JSON Schema validation");

const MAX_SCHEMA_NODES = 100_000;
const MAX_SCHEMA_DEPTH = 10_000;
const MCP_SUPPORTED_DIALECTS = new Set([
  "https://json-schema.org/draft/2020-12/schema",
  "http://json-schema.org/draft/2020-12/schema",
]);
const compiled = new Map();
const sessions = new Map();
let input;

if (import.meta.main) await main();

async function main() {
  input = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of input) {
    let request;
    try {
      request = JSON.parse(line);
      const reply = handle(request);
      process.stdout.write(`${JSON.stringify(reply)}\n`);
    } catch (error) {
      const id = request?.id;
      if (typeof id === "string") {
        const detail = exactText(error instanceof Error ? error.message : String(error));
        process.stdout.write(`${JSON.stringify({ id, kind: "worker_error", detail })}\n`);
      }
    }
  }
}

function handle(request) {
  if (request?.kind === "compile" && typeof request.schemaId === "string") {
    return compileSchema(request);
  }
  if (request?.kind === "validate" && typeof request.schemaId === "string") {
    return validateInput(request);
  }
  if (request?.kind === "release" && typeof request.schemaId === "string") {
    compiled.delete(request.schemaId);
    return { id: request.id, kind: "released" };
  }
  if (request?.kind === "release_session" && typeof request.sessionId === "string") {
    sessions.delete(request.sessionId);
    for (const [schemaId, entry] of compiled) {
      if (entry.sessionId === request.sessionId) compiled.delete(schemaId);
    }
    return { id: request.id, kind: "session_released" };
  }
  if (request?.kind === "shutdown") {
    input.close();
    return { id: request.id, kind: "stopped" };
  }
  return {
    id: request?.id,
    kind: "worker_error",
    detail: exactText("unknown JSON Schema worker request"),
  };
}

function compileSchema(request) {
  try {
    const schema = decodeSourceValue(request.schema);
    if (typeof request.sessionId !== "string" || request.sessionId.length === 0) {
      throw new Error("validator session id is missing");
    }
    if (request.profile !== "structured_output_draft7" && request.profile !== "mcp_output_schema2020") {
      throw new Error("unknown Native JSON Schema profile");
    }
    if (request.profile === "structured_output_draft7") {
      if (schemaIsTooLarge(schema)) {
        return { id: request.id, kind: "schema_error", detail: exactText("schema too large") };
      }
      if (Boolean(schema.$async)) {
        return {
          id: request.id,
          kind: "schema_error",
          detail: exactText("$async schemas are not supported"),
        };
      }
    }
    if (
      request.profile === "mcp_output_schema2020"
      && "$schema" in schema
      && typeof schema.$schema === "string"
      && !MCP_SUPPORTED_DIALECTS.has(schema.$schema.replace(/#$/, ""))
    ) {
      const dialect = schema.$schema.slice(0, 200);
      throw new Error(
        `JSON Schema declares an unsupported dialect ("$schema": "${dialect}"). The default validator supports JSON Schema 2020-12 only; pass a pre-configured Ajv instance to AjvJsonSchemaValidator(ajv) to validate other dialects.`,
      );
    }
    let session = sessions.get(request.sessionId);
    if (session && session.profile !== request.profile) {
      throw new Error("validator session profile does not match");
    }
    if (!session) {
      let ajv;
      if (request.profile === "structured_output_draft7") {
        ajv = new Ajv({ allErrors: true, validateFormats: false, multipleOfPrecision: 6 });
      } else {
        ajv = new Ajv2020({
          strict: false,
          validateFormats: true,
          validateSchema: false,
          allErrors: true,
        });
        addFormats(ajv);
      }
      session = { profile: request.profile, ajv };
      sessions.set(request.sessionId, session);
    }
    if (request.profile === "structured_output_draft7") {
      if (!session.ajv.validateSchema(schema)) {
        return {
          id: request.id,
          kind: "schema_error",
          detail: exactText(session.ajv.errorsText(session.ajv.errors)),
        };
      }
    }
    const validate = request.profile === "mcp_output_schema2020"
      && typeof schema.$id === "string"
      ? session.ajv.getSchema(schema.$id) ?? session.ajv.compile(schema)
      : session.ajv.compile(schema);
    compiled.set(request.schemaId, {
      ajv: session.ajv,
      validate,
      profile: request.profile,
      sessionId: request.sessionId,
    });
    return { id: request.id, kind: "compiled" };
  } catch (error) {
    return {
      id: request.id,
      kind: "schema_error",
      detail: exactText(error instanceof Error ? error.message : String(error)),
    };
  }
}

function validateInput(request) {
  const entry = compiled.get(request.schemaId);
  if (!entry) {
    return {
      id: request.id,
      kind: "worker_error",
      detail: exactText("compiled schema handle is unknown"),
    };
  }
  try {
    const data = decodeSourceValue(request.input);
    const result = entry.validate(data);
    // Native MCP's synchronous Ajv wrapper tests the validator return value
    // directly. An Ajv async-schema Promise is therefore truthy at that seam;
    // preserve that behavior instead of awaiting a different validation path.
    if (result && typeof result.then === "function") {
      result.catch(() => {});
      return { id: request.id, kind: "valid" };
    }
    if (result) return { id: request.id, kind: "valid" };
    const errors = entry.validate.errors ?? [];
    if (entry.profile === "mcp_output_schema2020") {
      return {
        id: request.id,
        kind: "invalid",
        detail: exactText(entry.ajv.errorsText(errors)),
        keywords: errors.map((error) => error.keyword).join(","),
      };
    }
    return {
      id: request.id,
      kind: "invalid",
      detail: exactText(errors.map((error) => formatValidationError(error, data)).join(", ")),
      keywords: errors.map((error) => error.keyword).join(","),
    };
  } catch (error) {
    return {
      id: request.id,
      kind: "worker_error",
      detail: exactText(error instanceof Error ? error.message : String(error)),
    };
  }
}

function schemaIsTooLarge(schema) {
  let count = 0;
  const pending = [[schema, 0]];
  while (pending.length > 0) {
    const [value, depth] = pending.pop();
    count += 1;
    if (count > MAX_SCHEMA_NODES || depth > MAX_SCHEMA_DEPTH) return true;
    if (value === null || typeof value !== "object") continue;
    const children = Array.isArray(value) ? value : Object.values(value);
    for (let index = children.length - 1; index >= 0; index -= 1) {
      pending.push([children[index], depth + 1]);
    }
  }
  return false;
}

export function decodeSourceValue(dto) {
  if (!dto || !Number.isInteger(dto.root) || !Array.isArray(dto.nodes)) {
    throw new Error("flat Source value is malformed");
  }
  const values = new Array(dto.nodes.length);
  for (let index = 0; index < dto.nodes.length; index += 1) {
    const node = dto.nodes[index];
    switch (node?.kind) {
      case "null":
        values[index] = null;
        break;
      case "bool":
        if (typeof node.value !== "boolean") throw new Error("flat bool node is malformed");
        values[index] = node.value;
        break;
      case "number":
        values[index] = numberFromBits(node.bitsHex);
        break;
      case "string":
        values[index] = stringFromUnits(node.codeUnits);
        break;
      case "array":
        if (!Array.isArray(node.items)) throw new Error("flat array node is malformed");
        values[index] = new Array(node.items.length);
        break;
      case "object":
        if (!Array.isArray(node.entries)) throw new Error("flat object node is malformed");
        // JSON.parse creates ordinary objects. defineProperty preserves the
        // own-data-property behavior of the special key "__proto__".
        values[index] = {};
        break;
      default:
        throw new Error("flat Source value has an unknown node kind");
    }
  }
  for (let index = 0; index < dto.nodes.length; index += 1) {
    const node = dto.nodes[index];
    if (node.kind === "array") {
      for (let child = 0; child < node.items.length; child += 1) {
        values[index][child] = childValue(values, node.items[child]);
      }
    } else if (node.kind === "object") {
      for (const property of node.entries) {
        if (!Array.isArray(property.keyUnits)) throw new Error("flat object key is malformed");
        Object.defineProperty(values[index], stringFromUnits(property.keyUnits), {
          value: childValue(values, property.value),
          enumerable: true,
          writable: true,
          configurable: true,
        });
      }
    }
  }
  if (dto.root < 0 || dto.root >= values.length) throw new Error("flat root index is invalid");
  return values[dto.root];
}

function childValue(values, index) {
  if (!Number.isInteger(index) || index < 0 || index >= values.length) {
    throw new Error("flat child index is invalid");
  }
  return values[index];
}

function numberFromBits(hex) {
  if (typeof hex !== "string" || !/^[0-9a-f]{16}$/.test(hex)) {
    throw new Error("flat number bits are malformed");
  }
  const view = new DataView(new ArrayBuffer(8));
  view.setBigUint64(0, BigInt(`0x${hex}`), false);
  return view.getFloat64(0, false);
}

function stringFromUnits(units) {
  if (!Array.isArray(units) || units.some((unit) => !Number.isInteger(unit) || unit < 0 || unit > 0xffff)) {
    throw new Error("flat UTF-16 units are malformed");
  }
  let output = "";
  for (let index = 0; index < units.length; index += 4096) {
    output += String.fromCharCode(...units.slice(index, index + 4096));
  }
  return output;
}

function exactText(text) {
  const codeUnits = [];
  for (let index = 0; index < text.length; index += 1) {
    codeUnits.push(text.charCodeAt(index));
  }
  return { display: displayFromUnits(codeUnits), codeUnits };
}

function displayFromUnits(units) {
  const safeUnits = [];
  for (let index = 0; index < units.length; index += 1) {
    const unit = units[index];
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const next = units[index + 1];
      if (next >= 0xdc00 && next <= 0xdfff) {
        safeUnits.push(unit, next);
        index += 1;
      } else {
        safeUnits.push(0xfffd);
      }
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      safeUnits.push(0xfffd);
    } else {
      safeUnits.push(unit);
    }
  }
  return stringFromUnits(safeUnits);
}

function pointerValue(root, instancePath) {
  let value = root;
  const segments = instancePath.split("/").slice(1);
  for (const encoded of segments) {
    const key = encoded.replaceAll("~1", "/").replaceAll("~0", "~");
    if (Array.isArray(value)) value = value[Number(key)];
    else if (value !== null && typeof value === "object" && Object.hasOwn(value, key)) value = value[key];
    else return undefined;
  }
  return value;
}

function jsonSummary(value, cap) {
  try {
    const text = JSON.stringify(value);
    if (typeof text !== "string") return undefined;
    return text.length > cap ? `${truncatePrefix(text, cap)}…` : text;
  } catch {
    return undefined;
  }
}

function truncatePrefix(text, limit) {
  if (limit <= 0) return "";
  if (text.length <= limit) return text;
  const prefix = text.slice(0, limit);
  const last = prefix.charCodeAt(limit - 1);
  return last >= 0xd800 && last <= 0xdbff ? prefix.slice(0, -1) : prefix;
}

function formatValidationError(error, data) {
  const prefix = `${error.instancePath || "root"}: ${error.message}`;
  switch (error.keyword) {
    case "additionalProperties": {
      const key = error.params.additionalProperty;
      return typeof key === "string"
        ? `${prefix} ('${key.length > 80 ? `${truncatePrefix(key, 80)}…` : key}' is not allowed)`
        : prefix;
    }
    case "minLength":
    case "maxLength": {
      const value = pointerValue(data, error.instancePath);
      return typeof value === "string" ? `${prefix} (got ${[...value].length})` : prefix;
    }
    case "minItems":
    case "maxItems": {
      const value = pointerValue(data, error.instancePath);
      return Array.isArray(value) ? `${prefix} (got ${value.length})` : prefix;
    }
    case "minProperties":
    case "maxProperties": {
      const value = pointerValue(data, error.instancePath);
      return value !== null && typeof value === "object" && !Array.isArray(value)
        ? `${prefix} (got ${Object.keys(value).length})`
        : prefix;
    }
    case "enum": {
      const summary = jsonSummary(error.params.allowedValues, 300);
      return summary === undefined ? prefix : `${prefix}: ${summary}`;
    }
    case "const": {
      const summary = jsonSummary(error.params.allowedValue, 300);
      return summary === undefined ? prefix : `${prefix}: ${summary}`;
    }
    default:
      return prefix;
  }
}
