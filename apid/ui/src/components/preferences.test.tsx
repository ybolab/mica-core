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
  it('changes and persists the language while synchronizing the document', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('combobox', { name: 'Language' }))
    await userEvent.click(await screen.findByRole('option', { name: '简体中文' }))

    await waitFor(() => expect(document.documentElement.lang).toBe('zh-CN'))
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('zh-CN')
    expect(screen.getByRole('combobox', { name: '外观' })).toBeTruthy()
  })

  it('changes and persists an explicit theme', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('combobox', { name: 'Appearance' }))
    await userEvent.click(await screen.findByRole('option', { name: 'Dark' }))

    await waitFor(() => expect(document.documentElement.classList.contains('dark')).toBe(true))
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark')
  })
})
