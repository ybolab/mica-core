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

async function openPicker() {
  const input = screen.getByRole('combobox', { name: 'Language' })
  await userEvent.click(input)
  return input
}

describe('display preferences', () => {
  it('searches the picker, applies a language and persists the choice', async () => {
    renderPreferences()

    const input = await openPicker()
    await userEvent.type(input, 'chinese')
    await userEvent.click(await screen.findByRole('option', { name: /简体中文/ }))

    await waitFor(() => expect(document.documentElement.lang).toBe('zh-CN'))
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('zh-CN')
  })

  /// A language the console has no bundle for is still offered, labelled, and
  /// still recorded as the operator's choice. It renders in English rather
  /// than silently reverting to whatever the browser prefers.
  it('marks a language it cannot render and falls back to English', async () => {
    renderPreferences()

    await openPicker()
    const japanese = await screen.findByRole('option', { name: /日本語/ })
    expect(japanese.textContent).toContain('Planned')
    await userEvent.click(japanese)

    await waitFor(() => expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('ja'))
    expect(document.documentElement.lang).toBe('en')
  })

  it('says when no language matches the search', async () => {
    renderPreferences()

    const input = await openPicker()
    await userEvent.type(input, 'klingon')

    expect(await screen.findByText('No languages match')).toBeTruthy()
  })

  it('changes and persists an explicit theme', async () => {
    renderPreferences()

    await userEvent.click(screen.getByRole('button', { name: 'Dark' }))

    await waitFor(() => expect(document.documentElement.classList.contains('dark')).toBe(true))
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark')
    expect(screen.getByRole('button', { name: 'Dark' }).getAttribute('aria-pressed')).toBe('true')
  })

  /// The picker used to be a dialog rendered inside the header's dropdown menu,
  /// which left the menu open on top of its own backdrop. It anchors to its own
  /// trigger now, so there is no second layer to leave behind.
  it('opens no dialog layer of its own', async () => {
    renderPreferences()

    await openPicker()

    expect(await screen.findByRole('listbox')).toBeTruthy()
    expect(screen.queryByRole('dialog')).toBeNull()
  })
})
