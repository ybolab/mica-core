import { describe, expect, it } from 'vitest'
import { detectLocale, normalizeLocale } from './locale'
import { en, zhCN } from './resources'

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

  it('keeps English and Chinese resource keys in exact parity', () => {
    expect(keyPaths(zhCN)).toEqual(keyPaths(en))
  })
})
