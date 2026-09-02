import { useTranslation } from 'react-i18next'
import type { AvailableFact } from '@/lib/types'

/// An absent fact rendered as absence with its reason. It is deliberately not
/// an error tone and never a zero: the device could not observe this, which is
/// a third thing from "healthy" and from "failed".
export function Unavailable({ fact }: { fact: AvailableFact }) {
  const { t } = useTranslation()
  return <span className="text-muted-foreground">{t('common.unavailable')}{fact.detail ? ` — ${fact.detail}` : ''}</span>
}

/// Joins the parts of a one-line summary, dropping the parts the device did
/// not report rather than printing placeholders for them.
export function join(values: (string | null | undefined | false)[]) {
  const present = values.filter((value): value is string => Boolean(value))
  return present.length > 0 ? present.join(' · ') : undefined
}

/// Binary units, because every number these surfaces report comes from a
/// block device or a file on one.
export function formatBytes(bytes: number) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`
}
