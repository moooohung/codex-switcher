import React from "react";
import ReactDOM from "react-dom/client";
import { ProviderTray } from "./components/ProviderTray";
import { syncThemeFromStorage } from "./lib/theme";
import "./App.css";

syncThemeFromStorage();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ProviderTray />
  </React.StrictMode>
);
