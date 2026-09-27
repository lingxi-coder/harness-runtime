import React, { useCallback, useMemo, useState } from "react";
import { useApp, useHostStyles } from "@modelcontextprotocol/ext-apps/react";

function pretty(value) {
  if (value == null) {
    return "null";
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch (_error) {
    return String(value);
  }
}

export function McpWidgetStarter() {
  const [status, setStatus] = useState("Waiting for MCP Apps host");
  const [toolInput, setToolInput] = useState(null);
  const [toolResult, setToolResult] = useState(null);
  const [hostContext, setHostContext] = useState(null);
  const [toolName, setToolName] = useState("replace_with_tool_name");
  const [toolArgsText, setToolArgsText] = useState('{\n  "example": true\n}');
  const [followUpPrompt, setFollowUpPrompt] = useState(
    "Summarize the current widget state for the user.",
  );
  const { app } = useApp({
    appInfo: {
      name: "lingxi-local-app-mcp-widget",
      version: "1.0.0",
    },
    capabilities: {},
    autoResize: true,
    onAppCreated(createdApp) {
      // Register handlers before the hook completes its connect flow.
      createdApp.onteardown = async () => {
        setStatus("Host requested teardown");
        return {};
      };
      createdApp.ontoolinput = (input) => {
        setToolInput(input ?? null);
        setStatus("Received tool input");
      };
      createdApp.ontoolresult = (result) => {
        setToolResult(result ?? null);
        setStatus(result?.isError ? "Received tool error" : "Received tool result");
      };
      createdApp.onhostcontextchanged = (context) => {
        setHostContext(context ?? null);
        setStatus("Connected to MCP Apps host");
      };
      createdApp.onerror = (error) => {
        setStatus(`Bridge error: ${pretty(error)}`);
      };
    },
  });
  useHostStyles(app, app?.getHostContext());

  const structuredContent = useMemo(() => {
    if (toolResult && typeof toolResult === "object" && "structuredContent" in toolResult) {
      return toolResult.structuredContent;
    }
    return null;
  }, [toolResult]);

  const runTool = useCallback(async () => {
    if (!app) {
      setStatus("MCP Apps host is not connected yet");
      return;
    }
    let parsedArgs;
    try {
      parsedArgs = JSON.parse(toolArgsText);
    } catch (error) {
      setStatus(`Invalid tool arguments: ${error.message}`);
      return;
    }
    setStatus(`Calling ${toolName}`);
    try {
      const result = await app.callServerTool({
        name: toolName,
        arguments: parsedArgs,
      });
      setToolResult(result);
      setStatus(`Tool ${toolName} completed`);
    } catch (error) {
      setStatus(`Tool call failed: ${error.message}`);
    }
  }, [app, toolArgsText, toolName]);

  const sendMessage = useCallback(async () => {
    if (!app) {
      setStatus("MCP Apps host is not connected yet");
      return;
    }
    setStatus("Sending follow-up message");
    try {
      await app.sendMessage({
        role: "user",
        content: [{ type: "text", text: followUpPrompt }],
      });
      setStatus("Follow-up message sent");
    } catch (error) {
      setStatus(`Follow-up failed: ${error.message}`);
    }
  }, [app, followUpPrompt]);

  return (
    <main
      style={{
        fontFamily: "Inter, system-ui, sans-serif",
        padding: 16,
        color: "#101828",
        background: "#f8fafc",
        minHeight: "100vh",
        boxSizing: "border-box",
      }}
    >
      <section
        style={{
          maxWidth: 720,
          margin: "0 auto",
          background: "#ffffff",
          border: "1px solid #e2e8f0",
          borderRadius: 16,
          padding: 20,
          boxShadow: "0 8px 30px rgba(15, 23, 42, 0.08)",
        }}
      >
        <h1 style={{ marginTop: 0, fontSize: 24 }}>Local App MCP widget starter</h1>
        <p style={{ marginTop: 0, color: "#475467" }}>
          Edit this widget after MCP authoring is enabled for the app. It starts
          from the MCP Apps bridge, not ChatGPT-only aliases.
        </p>

        <div
          style={{
            display: "grid",
            gap: 12,
            gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))",
          }}
        >
          <label style={{ display: "grid", gap: 6 }}>
            <span>Bridge status</span>
            <output
              style={{
                padding: "10px 12px",
                borderRadius: 12,
                background: app ? "#ecfdf3" : "#fef3f2",
                color: app ? "#027a48" : "#b42318",
              }}
            >
              {status}
            </output>
          </label>

          <label style={{ display: "grid", gap: 6 }}>
            <span>Tool name</span>
            <input value={toolName} onChange={(event) => setToolName(event.target.value)} />
          </label>
        </div>

        <div style={{ display: "grid", gap: 12, marginTop: 16 }}>
          <label style={{ display: "grid", gap: 6 }}>
            <span>Tool arguments JSON</span>
            <textarea
              rows={8}
              value={toolArgsText}
              onChange={(event) => setToolArgsText(event.target.value)}
            />
          </label>

          <label style={{ display: "grid", gap: 6 }}>
            <span>Follow-up prompt</span>
            <textarea
              rows={3}
              value={followUpPrompt}
              onChange={(event) => setFollowUpPrompt(event.target.value)}
            />
          </label>
        </div>

        <div style={{ display: "flex", gap: 12, marginTop: 16, flexWrap: "wrap" }}>
          <button disabled={!app} onClick={runTool} type="button">
            Call tool
          </button>
          <button disabled={!app} onClick={sendMessage} type="button">
            Send ui/message
          </button>
        </div>

        <div
          style={{
            display: "grid",
            gap: 16,
            marginTop: 20,
            gridTemplateColumns: "repeat(auto-fit, minmax(260px, 1fr))",
          }}
        >
          <article>
            <h2 style={{ fontSize: 18 }}>Tool input</h2>
            <pre style={{ whiteSpace: "pre-wrap" }}>{pretty(toolInput)}</pre>
          </article>
          <article>
            <h2 style={{ fontSize: 18 }}>Structured content</h2>
            <pre style={{ whiteSpace: "pre-wrap" }}>{pretty(structuredContent)}</pre>
          </article>
        </div>

        <article style={{ marginTop: 16 }}>
          <h2 style={{ fontSize: 18 }}>Host context</h2>
          <pre style={{ whiteSpace: "pre-wrap" }}>{pretty(hostContext)}</pre>
        </article>

        <article style={{ marginTop: 16 }}>
          <h2 style={{ fontSize: 18 }}>Raw tool result</h2>
          <pre style={{ whiteSpace: "pre-wrap" }}>{pretty(toolResult)}</pre>
        </article>
      </section>
    </main>
  );
}
