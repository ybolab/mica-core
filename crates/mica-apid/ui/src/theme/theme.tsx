import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react'

export type ThemeMode = 'system' | 'light' | 'dark'
export type ResolvedTheme = Exclude<ThemeMode, 'system'>

export const THEME_STORAGE_KEY = 'mica.ui.theme'

/// The value `<meta name="theme-color">` falls back to before the stylesheet
/// has resolved, or in a test environment that computes no styles. The live
/// value is read from `--background` below; a second copy of the palette here
/// would agree with itself while the tokens moved.
const FALLBACK_THEME_COLOR = 'oklch(0.9603 0.008 98.88)'

export function normalizeThemeMode(value: string | null | undefined): ThemeMode {
  return value === 'light' || value === 'dark' || value === 'system' ? value : 'system'
}

export function resolveTheme(mode: ThemeMode, prefersDark: boolean): ResolvedTheme {
  return mode === 'system' ? prefersDark ? 'dark' : 'light' : mode
}

function safeStoredTheme(): ThemeMode {
  try {
    return normalizeThemeMode(window.localStorage.getItem(THEME_STORAGE_KEY))
  } catch {
    return 'system'
  }
}

function prefersDark(): boolean {
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? false
}

export function applyResolvedTheme(theme: ResolvedTheme, rootDocument: Document = document) {
  const root = rootDocument.documentElement
  root.classList.remove('light', 'dark')
  root.classList.add(theme)
  root.style.colorScheme = theme
  let meta = rootDocument.head.querySelector<HTMLMetaElement>('meta[name="theme-color"]')
  if (!meta) {
    meta = rootDocument.createElement('meta')
    meta.name = 'theme-color'
    rootDocument.head.append(meta)
  }
  // Read back rather than restated: the browser chrome cannot resolve a custom
  // property itself, but it can be told what this document resolved it to.
  const resolved = rootDocument.defaultView?.getComputedStyle(root).getPropertyValue('--background').trim()
  meta.content = resolved || FALLBACK_THEME_COLOR
}

export function initializeTheme(): ThemeMode {
  const mode = safeStoredTheme()
  applyResolvedTheme(resolveTheme(mode, prefersDark()))
  return mode
}

interface ThemeContextValue {
  mode: ThemeMode
  resolvedTheme: ResolvedTheme
  setMode: (mode: ThemeMode) => void
}

const ThemeContext = createContext<ThemeContextValue | undefined>(undefined)

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [mode, setModeState] = useState<ThemeMode>(safeStoredTheme)
  const [systemDark, setSystemDark] = useState(prefersDark)
  const resolvedTheme = resolveTheme(mode, systemDark)

  useEffect(() => {
    const media = window.matchMedia?.('(prefers-color-scheme: dark)')
    if (!media) return
    const sync = (event: MediaQueryListEvent | MediaQueryList) => setSystemDark(event.matches)
    sync(media)
    if (mode !== 'system') return
    media.addEventListener('change', sync)
    return () => media.removeEventListener('change', sync)
  }, [mode])

  useEffect(() => {
    applyResolvedTheme(resolvedTheme)
  }, [resolvedTheme])

  const value = useMemo<ThemeContextValue>(() => ({
    mode,
    resolvedTheme,
    setMode: (nextMode) => {
      try {
        window.localStorage.setItem(THEME_STORAGE_KEY, nextMode)
      } catch {
        // Browser privacy settings may disable local storage; the session still updates.
      }
      setModeState(nextMode)
    },
  }), [mode, resolvedTheme])

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
}

export function useTheme() {
  const context = useContext(ThemeContext)
  if (!context) throw new Error('useTheme must be used inside ThemeProvider')
  return context
}
