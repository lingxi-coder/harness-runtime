import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "@/app/app";
import { AppProviders } from "@/app/providers";
import "@/app/globals.css";

const rootElement = document.getElementById("root");
if (!rootElement) {
  throw new Error("LingXi local app root element is missing");
}

createRoot(rootElement).render(
  <React.StrictMode>
    <AppProviders>
      <App />
    </AppProviders>
  </React.StrictMode>,
);
