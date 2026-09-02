import { I18nextProvider } from 'react-i18next'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { i18n } from '@/i18n/i18n'
import { LOCALE_STORAGE_KEY } from '@/i18n/locale'
import { ThemeProvider, THEME_STORAGE_KEY } from '@/theme/theme'
import { Preferences } from './preferences'

beforeEach(async () => {
  window.localStorage.clear()
  document.documentElement.className = ''
  document.documentElement.lang = 'en'
  vi.stubGlobal('matchMedia', vi.fn().mockImplementation(() => ({
    matches: false,
    media: '(prefers-color-scheme: dark)',
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  })))
  await i18n.changeLanguage('en')
})

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function renderPreferences() {
  return render(
    <I18nextProvider i18n={i18n}>
      <ThemeProvider>
        <Preferences />
      </ThemeProvider>
    </I18nextProvider>,
  )
}

describe('display preferences', () => {
  it('searches the picker, applies a language and persists the choice', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('button', { name: 'Language' }))
    await userEvent.type(await screen.findByRole('searchbox', { name: 'Search languages' }), 'chinese')
    await userEvent.click(screen.getByRole('button', { name: /简体中文/ }))

    await waitFor(() => expect(document.documentElement.lang).toBe('zh-CN'))
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('zh-CN')
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  /// A language the console has no bundle for is still offered, labelled, and
  /// still recorded as the operator's choice. It renders in English rather
  /// than silently reverting to whatever the browser prefers.
  it('marks a language it cannot render and falls back to English', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('button', { name: 'Language' }))
    const japanese = screen.getByRole('button', { name: /日本語/ })
    expect(japanese.textContent).toContain('Planned')
    await userEvent.click(japanese)

    await waitFor(() => expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('ja'))
    expect(document.documentElement.lang).toBe('en')
  })

  it('says when no language matches the search', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('button', { name: 'Language' }))
    await userEvent.type(await screen.findByRole('searchbox', { name: 'Search languages' }), 'klingon')

    expect(screen.getByText('No languages match')).toBeTruthy()
  })

  it('changes and persists an explicit theme', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('radio', { name: 'Dark' }))

    await waitFor(() => expect(document.documentElement.classList.contains('dark')).toBe(true))
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark')
    expect(screen.getByRole('radio', { name: 'Dark' }).getAttribute('aria-checked')).toBe('true')
  })
})
