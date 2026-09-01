import { afterEach, describe, expect, it } from 'vitest'
import {
  applyResolvedTheme,
  normalizeThemeMode,
  resolveTheme,
} from './theme'

afterEach(() => {
  document.documentElement.className = ''
  document.documentElement.style.colorScheme = ''
  document.head.querySelector('meta[name="theme-color"]')?.remove()
})

describe('theme preferences', () => {
  it('accepts only supported persisted modes', () => {
    expect(normalizeThemeMode('light')).toBe('light')
    expect(normalizeThemeMode('dark')).toBe('dark')
    expect(normalizeThemeMode('system')).toBe('system')
    expect(normalizeThemeMode('darkest')).toBe('system')
    expect(normalizeThemeMode(null)).toBe('system')
  })

  it('resolves system mode without overriding an explicit selection', () => {
    expect(resolveTheme('system', true)).toBe('dark')
    expect(resolveTheme('system', false)).toBe('light')
    expect(resolveTheme('light', true)).toBe('light')
    expect(resolveTheme('dark', false)).toBe('dark')
  })

  it('synchronizes the root class, color scheme and browser chrome color', () => {
    const meta = document.createElement('meta')
    meta.name = 'theme-color'
    document.head.append(meta)

    applyResolvedTheme('dark')

    expect(document.documentElement.classList.contains('dark')).toBe(true)
    expect(document.documentElement.classList.contains('light')).toBe(false)
    expect(document.documentElement.style.colorScheme).toBe('dark')
    expect(meta.content).toBe('#1d1d1d')

    applyResolvedTheme('light')

    expect(document.documentElement.classList.contains('light')).toBe(true)
    expect(document.documentElement.classList.contains('dark')).toBe(false)
    expect(meta.content).toBe('#f8f8f8')
  })
})
