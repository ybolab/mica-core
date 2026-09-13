import { toast as manager } from '@/shared/components/ui/toast'
import { ApiError } from '@/shared/lib/http'

/// The console's one feedback channel.
///
/// Before this existed, an outcome was reported by whichever inline paragraph
/// the page happened to render, which meant most writes reported nothing at all
/// on success and several reported their failures into a shared line that named
/// no action. Callers now say what happened; where it appears is not their
/// decision.
///
/// Device *state* does not belong here. A staged reset or a required credential
/// rotation has to survive a page revisit, and a toast must not, so those stay
/// as an inline `Callout`.

const SUCCESS_TIMEOUT = 4_000
/// A failure is read, not glanced at: it carries the device's own sentence and
/// the operator may need to copy it.
const FAILURE_TIMEOUT = 10_000

export function notifySuccess(title: string, description?: string) {
  manager.add({ title, description, type: 'success', timeout: SUCCESS_TIMEOUT })
}

export function notifyFailure(title: string, description?: string) {
  manager.add({ title, description, type: 'error', timeout: FAILURE_TIMEOUT })
}

/// The device's own message when it sent one, and the transport's otherwise.
///
/// `ApiError.message` is already the daemon's `error.message` where the response
/// carried one, so a refusal reaches the operator in the daemon's words — which
/// name the offending value — rather than as a restated status code.
export function failureDetail(error: unknown, fallback: string) {
  if (error instanceof ApiError) return error.message
  return error instanceof Error ? error.message : fallback
}
