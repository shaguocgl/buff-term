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

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <I18nProvider>
      <App />
    </I18nProvider>
  </React.StrictMode>,
);
