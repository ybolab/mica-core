import { act, cleanup, render, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ThemeProvider } from './theme'

let onChange: ((event: MediaQueryListEvent) => void) | undefined

beforeEach(() => {
  window.localStorage.clear()
  document.documentElement.className = ''
  onChange = undefined
  vi.stubGlobal('matchMedia', vi.fn().mockImplementation(() => ({
    matches: false,
    media: '(prefers-color-scheme: dark)',
    addEventListener: vi.fn((_type: string, listener: (event: MediaQueryListEvent) => void) => {
      onChange = listener
    }),
    removeEventListener: vi.fn(),
  })))
})

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('ThemeProvider', () => {
  it('tracks operating-system changes while the selected mode is system', async () => {
    render(<ThemeProvider><span>content</span></ThemeProvider>)

    await waitFor(() => expect(document.documentElement.classList.contains('light')).toBe(true))
    expect(onChange).toBeTypeOf('function')

    act(() => onChange?.({ matches: true } as MediaQueryListEvent))

    await waitFor(() => expect(document.documentElement.classList.contains('dark')).toBe(true))
  })
})
