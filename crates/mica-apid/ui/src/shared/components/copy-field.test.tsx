import { cleanup, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderPanel } from '@/shared/testing/panel'
import { CopyField } from './copy-field'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the copy field', () => {
  it('copies the value and confirms it', async () => {
    const writeText = vi.fn(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } })
    renderPanel(<CopyField value="mica_tok_abcdef" label="Copy" />)

    await userEvent.click(screen.getByRole('button', { name: 'Copy' }))

    expect(writeText).toHaveBeenCalledWith('mica_tok_abcdef')
    expect(await screen.findByText('Copied')).toBeTruthy()
  })

  /// The clipboard is permission-gated and refuses on an unfocused document.
  /// The previous spelling reported "Copied" regardless, so the operator
  /// pasted nothing and had no way to know.
  it('reports a refused copy instead of claiming success', async () => {
    const writeText = vi.fn(() => Promise.reject(new Error('Document is not focused')))
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } })
    renderPanel(<CopyField value="mica_tok_abcdef" label="Copy" />)

    await userEvent.click(screen.getByRole('button', { name: 'Copy' }))

    expect(await screen.findByText('The value could not be copied to the clipboard.')).toBeTruthy()
    expect(screen.queryByText('Copied')).toBeNull()
  })
})
