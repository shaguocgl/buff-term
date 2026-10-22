import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { I18nProvider, LANG_STORAGE_KEY } from "./i18n";
import { IS_MAC } from "./utils/platform";

if (IS_MAC) {
  document.documentElement.classList.add("platform-mac");
}

// 在 React 渲染前应用已保存的主题，避免日间模式下首帧闪烁暗色主题
try {
  if (localStorage.getItem("buffterm-theme") === "light") {
    document.documentElement.setAttribute("data-theme", "light");
  }
} catch {
  /* ignore storage errors */
}

// 同理应用已保存的语言标记：英文界面依赖 :lang(en) 调整部分控件宽度，
// 提前设置可避免首帧按中文宽度排版后跳变
try {
  if (localStorage.getItem(LANG_STORAGE_KEY) === "en") {
    document.documentElement.lang = "en";
  }
} catch {
  /* ignore storage errors */
}

class AppErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (!this.state.error) return this.props.children;
    const en = document.documentElement.lang === "en";
    return (
      <div
        style={{
          padding: 32,
          color: "#e8eaf0",
          background: "#0b0d12",
          minHeight: "100vh",
          fontFamily: "system-ui, sans-serif",
        }}
      >
        <h1 style={{ fontSize: 20 }}>buffTerm</h1>
        <p style={{ color: "#f5a623" }}>
          {en
            ? "The interface ran into an unexpected error."
            : "界面遇到了未预期的错误。"}
        </p>
        <pre
          style={{
            whiteSpace: "pre-wrap",
            color: "#9aa4b2",
            fontSize: 12,
            maxWidth: 900,
          }}
        >
          {this.state.error.message}
        </pre>
        <button
          onClick={() => window.location.reload()}
          style={{ padding: "8px 14px", cursor: "pointer" }}
        >
          {en ? "Reload" : "重新加载"}
        </button>
      </div>
    );
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <AppErrorBoundary>
      <I18nProvider>
        <App />
      </I18nProvider>
    </AppErrorBoundary>
  </React.StrictMode>,
);
