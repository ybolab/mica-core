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

  it('synchronizes the root class and the color scheme', () => {
    applyResolvedTheme('dark')

    expect(document.documentElement.classList.contains('dark')).toBe(true)
    expect(document.documentElement.classList.contains('light')).toBe(false)
    expect(document.documentElement.style.colorScheme).toBe('dark')

    applyResolvedTheme('light')

    expect(document.documentElement.classList.contains('light')).toBe(true)
    expect(document.documentElement.classList.contains('dark')).toBe(false)
  })

  it('reads the browser chrome color back from the resolved token', () => {
    // Not restated from a second copy of the palette. Three self-consistent
    // copies of a colour agree with each other while the tokens move, and only
    // the browser chrome shows the drift.
    const meta = document.createElement('meta')
    meta.name = 'theme-color'
    document.head.append(meta)
    document.documentElement.style.setProperty('--background', 'oklch(0.12 0.03 271)')

    applyResolvedTheme('dark')

    expect(meta.content).toBe('oklch(0.12 0.03 271)')
  })

  it('falls back to a literal only when nothing resolves the token', () => {
    const meta = document.createElement('meta')
    meta.name = 'theme-color'
    document.head.append(meta)
    document.documentElement.style.removeProperty('--background')

    applyResolvedTheme('light')

    expect(meta.content).toBe('oklch(0.9603 0.008 98.88)')
  })
})
