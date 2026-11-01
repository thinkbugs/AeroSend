import React from "react";
import ReactDOM from "react-dom/client";
import "./styles/annotation.css";
import AnnotationOverlay from "./pages/AnnotationOverlay";

// A separate document on purpose: this window is transparent, so nothing from
// the main app's stylesheet — least of all its opaque body background — may
// reach it. Two Vite entries, two stylesheets, no shared head.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <AnnotationOverlay />
  </React.StrictMode>,
);
