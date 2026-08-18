import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

if (navigator.userAgent.includes("Mac")) {
  document.documentElement.classList.add("platform-mac");
}

// 在 React 渲染前应用已保存的主题，避免日间模式下首帧闪烁暗色主题
try {
  if (localStorage.getItem("keywisp-theme") === "light") {
    document.documentElement.setAttribute("data-theme", "light");
  }
} catch {
  /* ignore storage errors */
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
