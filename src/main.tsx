import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

if (navigator.userAgent.includes("Mac")) {
  document.documentElement.classList.add("platform-mac");
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
