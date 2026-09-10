import { useRef, useState, type ChangeEvent } from 'react'
import { Upload } from 'lucide-react'
import { Button } from '@/shared/components/ui/button'
import { Field, FieldDescription, FieldTitle } from '@/shared/components/ui/field'
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupText } from '@/shared/components/ui/input-group'
import { Progress, ProgressTrack, ProgressIndicator } from '@/shared/components/ui/progress'

/// A file chosen for upload, with the upload's own progress.
///
/// The raw `<input type="file">` it replaces rendered the browser's own control
/// — "Choose File / No file chosen", unstyled, untranslated — beside buttons
/// from the design system.
export function FilePicker({ label, hint, accept, chooseLabel, emptyLabel, submitLabel, pendingLabel, progress, pending, disabled, onSubmit }: {
  label: string
  hint?: string
  accept?: string
  chooseLabel: string
  emptyLabel: string
  submitLabel: string
  pendingLabel: string
  /// 0-100 while an upload is running, undefined otherwise.
  progress?: number
  pending?: boolean
  disabled?: boolean
  onSubmit: (file: File) => void
}) {
  const input = useRef<HTMLInputElement>(null)
  const [file, setFile] = useState<File>()
  const choose = (event: ChangeEvent<HTMLInputElement>) => setFile(event.target.files?.[0])

  return (
    <Field>
      {/* The visible control is a button, not the hidden input, so this labels
          the group rather than pointing `for` at an element the operator never
          sees. */}
      <FieldTitle>{label}</FieldTitle>
      <div className="flex flex-wrap items-center gap-3">
        <InputGroup className="min-w-64 flex-1">
          <InputGroupText className="min-w-0 flex-1 truncate px-3 text-left">
            {file?.name ?? emptyLabel}
          </InputGroupText>
          <InputGroupAddon align="inline-end">
            <InputGroupButton disabled={pending} onClick={() => input.current?.click()}>{chooseLabel}</InputGroupButton>
          </InputGroupAddon>
        </InputGroup>
        <input ref={input} className="sr-only" type="file" accept={accept} onChange={choose} tabIndex={-1} aria-hidden="true" />
        <Button disabled={!file || pending || disabled} onClick={() => file && onSubmit(file)}>
          <Upload />{pending ? pendingLabel : submitLabel}
        </Button>
      </div>
      {progress !== undefined ? (
        <Progress value={progress} aria-label={pendingLabel}>
          <ProgressTrack><ProgressIndicator /></ProgressTrack>
        </Progress>
      ) : null}
      {hint ? <FieldDescription>{hint}</FieldDescription> : null}
    </Field>
  )
}
