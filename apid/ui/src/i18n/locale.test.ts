import { describe, expect, it } from 'vitest'
import type { TFunction } from 'i18next'
import { formatAge, formatKnownState } from './format'
import { loadLocale } from './load'
import { autoLanguageNative, detectLocale, languageChoices, normalizeLocale, resolveLocaleChoice } from './locale'
import { en } from './resources'

function keyPaths(value: Record<string, unknown>, prefix = ''): string[] {
  return Object.entries(value).flatMap(([key, child]) => {
    const path = prefix ? `${prefix}.${key}` : key
    return child && typeof child === 'object'
      ? keyPaths(child as Record<string, unknown>, path)
      : [path]
  }).sort()
}

describe('locale preferences', () => {
  it('normalizes every Chinese browser locale to Simplified Chinese', () => {
    expect(normalizeLocale('zh')).toBe('zh-CN')
    expect(normalizeLocale('zh-Hant-TW')).toBe('zh-CN')
    expect(normalizeLocale('en-GB')).toBe('en')
    expect(normalizeLocale('fr-FR')).toBeUndefined()
  })

  it('prefers a valid stored choice and falls back through navigator locales', () => {
    expect(detectLocale('en', ['zh-CN'])).toBe('en')
    expect(detectLocale('invalid', ['fr-FR', 'zh-TW'])).toBe('zh-CN')
    expect(detectLocale(null, ['fr-FR'])).toBe('en')
  })

  it('loads Chinese on demand while keeping exact key parity', async () => {
    const zhCN = await loadLocale('zh-CN')
    expect(keyPaths(zhCN)).toEqual(keyPaths(en))
  })

  it('translates known states and preserves device-specific values', () => {
    const translate = ((key: string) => `translated:${key}`) as unknown as TFunction
    expect(formatKnownState('running', translate)).toBe('translated:common.states.running')
    expect(formatKnownState('vendor-state', translate)).toBe('vendor-state')
  })
})

describe('language choices', () => {
  it('offers auto first, then every language the prototype lists', () => {
    const choices = languageChoices()
    expect(choices[0].id).toBe('auto')
    expect(choices.map((choice) => choice.id)).toContain('zh-CN')
    expect(choices.every((choice) => choice.native.length > 0 && choice.english.length > 0)).toBe(true)
  })

  /// A language the console cannot actually serve is offered, but it says so
  /// rather than looking like a shipped translation that failed to load.
  it('marks a language without a bundle as planned', () => {
    const choices = languageChoices()
    expect(choices.find((choice) => choice.id === 'zh-CN')?.planned).toBe(false)
    expect(choices.find((choice) => choice.id === 'ja')?.planned).toBe(true)
    expect(choices.find((choice) => choice.id === 'auto')?.planned).toBe(false)
  })

  it('keeps a shipped choice and follows the browser for auto', () => {
    expect(resolveLocaleChoice('zh-CN', ['en-GB'])).toBe('zh-CN')
    expect(resolveLocaleChoice('auto', ['zh-TW'])).toBe('zh-CN')
  })

  /// Choosing a planned language must not leave the console in whatever the
  /// browser happens to prefer; the prototype falls back to English.
  it('resolves a planned language to English', () => {
    expect(resolveLocaleChoice('ja', ['zh-TW'])).toBe('en')
  })

  it('treats an unrecognized stored value as no choice at all', () => {
    expect(resolveLocaleChoice('not-a-language', ['zh-TW'])).toBe('zh-CN')
    expect(resolveLocaleChoice(null, ['fr-FR'])).toBe('en')
  })
})

describe('the automatic language label', () => {
  /// The picker says which language `auto` currently resolves to, and it names
  /// it in that language even when the console cannot render it yet.
  /// It reads the base language, not the region, because that is what the
  /// console resolves too: a zh-TW browser renders as Simplified Chinese, so
  /// labelling the choice 繁體中文 would describe a rendering that never happens.
  it('names the browser language natively', () => {
    expect(autoLanguageNative(['zh-TW'])).toBe('简体中文')
    expect(autoLanguageNative(['ja-JP'])).toBe('日本語')
    expect(autoLanguageNative(['en-GB'])).toBe('English')
  })

  it('falls back to English for a language it does not list', () => {
    expect(autoLanguageNative(['xh-ZA'])).toBe('English')
    expect(autoLanguageNative([])).toBe('English')
  })
})

describe('relative ages', () => {
  const translate = ((key: string, options?: { count?: number }) =>
    options?.count === undefined ? key : `${key}:${options.count}`) as unknown as TFunction

  /// Bucketed like the shell's freshness, and for the same reason: a table of
  /// live second counters cannot be screenshotted or asserted.
  it('buckets an age into just now, minutes and hours', () => {
    expect(formatAge(0, translate)).toBe('common.age.now')
    expect(formatAge(90_000, translate)).toBe('common.age.minutes:1')
    expect(formatAge(7_200_000, translate)).toBe('common.age.hours:2')
  })

  it('reads an unparseable timestamp as no age at all', () => {
    expect(formatAge(Number.NaN, translate)).toBe('common.age.now')
  })
})
