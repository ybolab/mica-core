import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Maximize2, Minimize2, Minus, TerminalSquare, X } from 'lucide-react'
import { Button } from '@/shared/components/ui/button'
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
import { ScrollArea } from '@/shared/components/ui/scroll-area'
import { PlannedNotice } from '@/shared/simulation/planned'
import { cn } from '@/shared/lib/utils'

/// The prototype's terminal window, with its minimize, fullscreen and close
/// controls and its minimized pill. There is no device endpoint behind it, so
/// it shows a fixed transcript and takes no input: echoing typed commands back
/// with invented output would be the console making up device behavior.
///
/// The window is the shared `Dialog` rather than a hand-built backdrop; the
/// previous version declared `role="dialog"` on a div and trapped nothing, so
/// Tab left it immediately and Escape did nothing.
export function TerminalWindow({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { t } = useTranslation()
  const [view, setView] = useState<'normal' | 'full' | 'minimized'>('normal')

  if (!open) return null

  if (view === 'minimized') {
    return (
      <div className="fixed right-4 bottom-16 z-35 flex items-center overflow-hidden rounded-lg border border-terminal-border bg-terminal text-terminal-foreground shadow-lg">
        <button type="button" className="flex min-h-11 items-center gap-2.5 px-3.5 text-sm font-semibold" onClick={() => setView('normal')}>
          <span className="size-2 rounded-full bg-terminal-accent" />
          <TerminalSquare className="size-4" aria-hidden="true" />
          <span>{t('services.terminal.title')}</span>
        </button>
        <Button variant="ghost" size="icon-sm" className="rounded-none border-l border-terminal-border text-terminal-foreground" onClick={onClose} aria-label={t('services.terminal.end')}><X /></Button>
      </div>
    )
  }

  return (
    <Dialog open onOpenChange={(next) => { if (!next) setView('minimized') }}>
      <DialogContent
        showCloseButton={false}
        className={cn(
          'border border-terminal-border bg-terminal text-terminal-foreground',
          view === 'full' ? 'h-[calc(100dvh-2rem)] sm:max-w-[calc(100vw-2rem)]' : 'sm:max-w-3xl',
        )}
        aria-label={t('services.terminal.title')}
      >
        <DialogHeader className="flex-row items-center gap-2 border-b border-terminal-border pb-3">
          <DialogTitle className="text-terminal-foreground">{t('services.terminal.title')}</DialogTitle>
          <DialogDescription className="min-w-0 truncate font-mono text-terminal-foreground/60">{t('services.terminal.endpoint')}</DialogDescription>
          <div className="ml-auto flex flex-none gap-1">
            <Button variant="ghost" size="icon-sm" className="text-terminal-foreground" onClick={() => setView('minimized')} aria-label={t('services.terminal.minimize')}><Minus /></Button>
            <Button variant="ghost" size="icon-sm" className="text-terminal-foreground" onClick={() => setView(view === 'full' ? 'normal' : 'full')} aria-label={t(view === 'full' ? 'services.terminal.restore' : 'services.terminal.fullscreen')}>{view === 'full' ? <Minimize2 /> : <Maximize2 />}</Button>
            <Button variant="ghost" size="icon-sm" className="text-terminal-foreground" onClick={onClose} aria-label={t('services.terminal.end')}><X /></Button>
          </div>
        </DialogHeader>
        <DialogBody className="flex flex-col gap-3">
          <PlannedNotice>{t('services.terminal.planned')}</PlannedNotice>
          <ScrollArea className="min-h-52 flex-1">
            <pre className="font-mono text-[0.8125rem] leading-relaxed">{TRANSCRIPT}</pre>
          </ScrollArea>
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

const TRANSCRIPT = `mos@device:~$ systemctl --no-pager status micad
● micad.service - mos settings daemon
   Active: active (running)
   Tasks: 8 (limit: 3834)
mos@device:~$ `
