import React from "react";
import { createRoot } from "react-dom/client";
import { McpWidgetStarter } from "./widget.jsx";

const rootElement = document.getElementById("root");
if (!rootElement) {
  throw new Error("MCP widget root element is missing");
}

createRoot(rootElement).render(
  <React.StrictMode>
    <McpWidgetStarter />
  </React.StrictMode>,
);
