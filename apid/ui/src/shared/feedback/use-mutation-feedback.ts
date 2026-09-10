import { useMutation, type UseMutationOptions } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { failureDetail, notifyFailure, notifySuccess } from './toast'

interface FeedbackOptions<TData, TVariables> extends Omit<UseMutationOptions<TData, Error, TVariables>, 'onSuccess' | 'onError'> {
  /// What the operator is told when the device accepted the request. Required:
  /// a write with nothing to say on success is the defect this exists to close.
  success: string | ((data: TData, variables: TVariables) => string)
  /// The headline for a refusal. The device's own sentence goes underneath it,
  /// so this names the action rather than restating the error.
  failure: string
  onSuccess?: (data: TData, variables: TVariables) => void
  onError?: (error: Error, variables: TVariables) => void
}

/// A mutation that always reports its outcome.
///
/// Wrapping `useMutation` rather than documenting a convention is deliberate:
/// the success message is a required argument, so a caller cannot land a silent
/// write without deleting the wrapper.
export function useMutationFeedback<TData = unknown, TVariables = void>(
  { success, failure, onSuccess, onError, ...options }: FeedbackOptions<TData, TVariables>,
) {
  const { t } = useTranslation()
  return useMutation<TData, Error, TVariables>({
    ...options,
    onSuccess: (data, variables) => {
      notifySuccess(typeof success === 'function' ? success(data, variables) : success)
      onSuccess?.(data, variables)
    },
    onError: (error, variables) => {
      notifyFailure(failure, failureDetail(error, t('common.requestFailed')))
      onError?.(error, variables)
    },
  })
}
