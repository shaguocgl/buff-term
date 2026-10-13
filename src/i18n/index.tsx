import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import { setAppLanguage } from '../api';
import { zh, type Dict } from './zh';
import { en } from './en';

/** 界面语言：zh = 简体中文，en = English */
export type Lang = 'zh' | 'en';

/** 语言偏好本地存储键（首帧即生效，无需等待后端） */
export const LANG_STORAGE_KEY = 'buffterm-lang';

/** 翻译函数的键类型：字典中实际存在的键 */
export type DictKey = keyof Dict;

/** 文案插值参数：`{name}` 形式占位符 */
export type TParams = Record<string, string | number>;

export type TFunc = (key: DictKey, params?: TParams) => string;

const DICTS: Record<Lang, Dict> = { zh, en };

/** 读取已保存的语言偏好；无 / 非法值一律回退中文 */
export function readSavedLang(): Lang {
  try {
    return localStorage.getItem(LANG_STORAGE_KEY) === 'en' ? 'en' : 'zh';
  } catch {
    return 'zh';
  }
}

/** 把 `{key}` 占位符替换为参数值；未提供对应参数时原样保留 */
export function formatText(template: string, params?: TParams): string {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (raw, key: string) =>
    key in params ? String(params[key]) : raw,
  );
}

/** 按语言取词（供非 React 环境，如事件回调、工具函数使用） */
export function translate(lang: Lang, key: DictKey, params?: TParams): string {
  return formatText(DICTS[lang][key] ?? key, params);
}

interface I18nValue {
  /** 当前界面语言 */
  lang: Lang;
  /** 切换语言：更新本地存储、界面文案并同步到后端（AI 提示词与后端错误文案） */
  setLang: (lang: Lang) => void;
  /** 翻译函数 */
  t: TFunc;
}

const I18nContext = createContext<I18nValue>({
  lang: 'zh',
  setLang: () => {},
  t: (key, params) => translate('zh', key, params),
});

export function I18nProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(readSavedLang);

  // 同步 <html lang>：屏幕阅读器据此选择发音，浏览器也据此选择 CJK 字体与断行规则
  useEffect(() => {
    document.documentElement.lang = lang === 'en' ? 'en' : 'zh-CN';
  }, [lang]);

  // 启动时把界面语言同步给后端，保证 AI 提示词 / 后端错误文案与界面一致
  useEffect(() => {
    setAppLanguage(lang).catch(() => {});
    // 仅首次挂载同步：后续切换在 setLang 中同步
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const setLang = useCallback((next: Lang) => {
    setLangState(next);
    try {
      localStorage.setItem(LANG_STORAGE_KEY, next);
    } catch {
      /* ignore storage errors */
    }
    setAppLanguage(next).catch(() => {});
  }, []);

  const value = useMemo<I18nValue>(
    () => ({
      lang,
      setLang,
      t: (key, params) => translate(lang, key, params),
    }),
    [lang, setLang],
  );

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

/** 完整 i18n 上下文（语言 + 切换 + 翻译） */
export function useI18n(): I18nValue {
  return useContext(I18nContext);
}

/** 只要翻译函数时使用，避免订阅 lang 之外的字段 */
export function useT(): TFunc {
  return useContext(I18nContext).t;
}

export type { Dict };
export { zh, en };
