// The public, read-only Glance (SUPERVISOR.md 12.1): its own small bundle, served by the
// public listener at "/" with the redacted feed. No sign-in, no app code, no controls.
import React from "react";
import ReactDOM from "react-dom/client";
import { GlancePage } from "@/pages/supervisor/glance/GlancePage";
import "./index.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <GlancePage streamUrl="/public/glance/stream" publicView />
  </React.StrictMode>,
);
