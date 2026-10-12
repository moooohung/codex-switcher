import React from "react";
import ReactDOM from "react-dom/client";
import { ProviderTabs } from "./components/ProviderTabs";
import { WebAccessGate } from "./components/WebAccessGate";
import "./App.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <WebAccessGate><ProviderTabs /></WebAccessGate>
  </React.StrictMode>
);
