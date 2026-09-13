import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { i18n, currentLocale, currentLocaleChoice, setLocaleChoice } from './i18n'
import { AUTO_LOCALE, LOCALE_STORAGE_KEY } from './locale'

beforeEach(async () => {
  window.localStorage.clear()
  await setLocaleChoice(AUTO_LOCALE)
})

afterEach(() => {
  window.localStorage.clear()
})

describe('choosing a language', () => {
  it('loads the catalogue, retitles the document and records the choice', async () => {
    await setLocaleChoice('zh-CN')

    expect(currentLocale()).toBe('zh-CN')
    expect(currentLocaleChoice()).toBe('zh-CN')
    expect(document.documentElement.lang).toBe('zh-CN')
    expect(i18n.t('shell.nav.overview')).toBe('概览')
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('zh-CN')
  })

  /// A language the console has no bundle for is still the operator's recorded
  /// choice; it renders in English rather than reverting to the browser's
  /// preference, which would look like the choice was ignored.
  it('records a planned language and renders English for it', async () => {
    await setLocaleChoice('ja')

    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('ja')
    expect(currentLocale()).toBe('en')
    expect(document.documentElement.lang).toBe('en')
  })

  it('returns to the browser preference on auto', async () => {
    await setLocaleChoice('zh-CN')
    await setLocaleChoice(AUTO_LOCALE)

    expect(currentLocaleChoice()).toBe(AUTO_LOCALE)
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe(AUTO_LOCALE)
  })
})
