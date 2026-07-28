/** React entry point for the Henosis Tauri webview. */
import "@fontsource-variable/instrument-sans";
import "@fontsource-variable/manrope";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";

/** Required HTML mount point created by the Henosis document shell. */
const rootElement = document.getElementById("root");

if (!rootElement) {
  throw new Error("Henosis could not find its application root.");
}

createRoot(rootElement).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
