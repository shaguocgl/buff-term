import { useEffect, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import {
  WindowCloseIcon,
  WindowMaximizeIcon,
  WindowMinimizeIcon,
  WindowRestoreIcon,
} from './Icons';

/**
 * 判断当前窗口是否需要前端自绘窗口按钮。
 *
 * Windows 通过 tauri.windows.conf.json 关闭了原生边框（decorations: false），
 * 原生最小化 / 最大化 / 关闭按钮会一并消失，因此改由标签栏右侧自绘；
 * macOS 仍使用原生红绿灯（titleBarStyle: Overlay），这里返回 false，不做替换。
 */
export function useCustomWindowControls() {
  /** 是否需要自绘窗口按钮（= 窗口没有原生边框） */
  const [custom, setCustom] = useState(false);
  /** 窗口是否处于最大化状态，决定第二个按钮显示“最大化”还是“向下还原” */
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    const win = getCurrentWindow();
    let disposed = false;
    let unlisten: (() => void) | null = null;

    const syncMaximized = () => {
      win
        .isMaximized()
        .then((v) => {
          if (!disposed) setMaximized(v);
        })
        .catch(() => {});
    };

    win
      .isDecorated()
      .then((decorated) => {
        if (disposed || decorated) return undefined;
        setCustom(true);
        syncMaximized();
        // 最大化 / 还原、拖动窗口尺寸都会触发 resize，用它同步图标
        return win.onResized(syncMaximized);
      })
      .then((fn) => {
        if (!fn) return;
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => {});

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  return { custom, maximized };
}

/** 最大化 / 向下还原（同时供标签栏双击调用） */
export function toggleWindowMaximize() {
  getCurrentWindow()
    .toggleMaximize()
    .catch(() => {});
}

interface WindowControlsProps {
  /** 当前窗口是否处于最大化状态 */
  maximized: boolean;
}

/** 无边框窗口自绘的最小化 / 最大化 / 关闭按钮组 */
export default function WindowControls({ maximized }: WindowControlsProps) {
  return (
    <div className="window-controls">
      <button
        className="window-control"
        title="最小化"
        aria-label="最小化"
        onClick={() => {
          getCurrentWindow()
            .minimize()
            .catch(() => {});
        }}
      >
        <WindowMinimizeIcon />
      </button>
      <button
        className="window-control"
        title={maximized ? '向下还原' : '最大化'}
        aria-label={maximized ? '向下还原' : '最大化'}
        onClick={toggleWindowMaximize}
      >
        {maximized ? <WindowRestoreIcon /> : <WindowMaximizeIcon />}
      </button>
      <button
        className="window-control close"
        title="关闭"
        aria-label="关闭"
        onClick={() => {
          getCurrentWindow()
            .close()
            .catch(() => {});
        }}
      >
        <WindowCloseIcon />
      </button>
    </div>
  );
}
