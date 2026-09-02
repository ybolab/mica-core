import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Maximize2, Minimize2, Minus, TerminalSquare, X } from 'lucide-react'
import { Button } from '@/shared/components/ui/button'
import { PlannedNotice } from '@/shared/simulation/planned'

/// The prototype's terminal window, with its minimize, fullscreen and close
/// controls and its minimized pill. There is no device endpoint behind it, so
/// it shows a fixed transcript and takes no input: echoing typed commands back
/// with invented output would be the console making up device behavior.
export function TerminalWindow({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { t } = useTranslation()
  const [view, setView] = useState<'normal' | 'full' | 'minimized'>('normal')

  if (!open) return null

  if (view === 'minimized') {
    return (
      <div className="terminal-pill">
        <button type="button" onClick={() => setView('normal')}>
          <span className="terminal-pill-dot" />
          <TerminalSquare aria-hidden="true" />
          <span>{t('services.terminal.title')}</span>
        </button>
        <Button variant="ghost" size="icon-sm" onClick={onClose} aria-label={t('services.terminal.end')}><X /></Button>
      </div>
    )
  }

  return (
    <div className="terminal-backdrop" onClick={() => setView('minimized')}>
      <div className={`terminal-window${view === 'full' ? ' terminal-full' : ''}`} role="dialog" aria-modal="true" aria-label={t('services.terminal.title')} onClick={(event) => event.stopPropagation()}>
        <div className="terminal-bar">
          <strong>{t('services.terminal.title')}</strong>
          <span className="mono">{t('services.terminal.endpoint')}</span>
          <div className="terminal-controls">
            <Button variant="ghost" size="icon-sm" onClick={() => setView('minimized')} aria-label={t('services.terminal.minimize')}><Minus /></Button>
            <Button variant="ghost" size="icon-sm" onClick={() => setView(view === 'full' ? 'normal' : 'full')} aria-label={t(view === 'full' ? 'services.terminal.restore' : 'services.terminal.fullscreen')}>{view === 'full' ? <Minimize2 /> : <Maximize2 />}</Button>
            <Button variant="ghost" size="icon-sm" onClick={onClose} aria-label={t('services.terminal.end')}><X /></Button>
          </div>
        </div>
        <div className="terminal-body">
          <PlannedNotice>{t('services.terminal.planned')}</PlannedNotice>
          <div className="terminal">
            <p>mos@device:~$ systemctl --no-pager status mosd</p>
            <p>● mosd.service - mos settings daemon</p>
            <p>&nbsp;&nbsp;&nbsp;Active: active (running)</p>
            <p>&nbsp;&nbsp;&nbsp;Tasks: 8 (limit: 3834)</p>
            <p className="terminal-cursor">mos@device:~$ </p>
          </div>
        </div>
      </div>
    </div>
  )
}
