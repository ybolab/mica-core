import { describe, expect, it } from 'vitest'
import { applyStage, stageState } from './apply-stage'

describe('interface apply stage', () => {
  it('is saved once the change is accepted and before the task is read', () => {
    expect(applyStage(undefined, 'connected')).toBe('saved')
  })

  it('is applying while the task is still running', () => {
    expect(applyStage({ status: 'running' }, 'connected')).toBe('applying')
    expect(applyStage({ status: 'queued' }, 'connected')).toBe('applying')
  })

  /// The step the prototype exists to show: the change took the device away
  /// from this browser. It is read from the shell's health state rather than
  /// assumed after a delay.
  it('is reconnecting while the device stops answering mid-apply', () => {
    expect(applyStage({ status: 'running' }, 'reconnecting')).toBe('reconnecting')
    expect(applyStage(undefined, 'offline')).toBe('reconnecting')
  })

  it('settles on the task outcome even if the device is still unreachable', () => {
    expect(applyStage({ status: 'finished', outcome: 'succeeded' }, 'offline')).toBe('applied')
    expect(applyStage({ status: 'finished', outcome: 'failed' }, 'connected')).toBe('failed')
  })
})

describe('apply strip steps', () => {
  it('marks the steps before the current one done and the current one active', () => {
    expect(stageState('saved', 'applying')).toBe('done')
    expect(stageState('applying', 'applying')).toBe('active')
    expect(stageState('reconnecting', 'applying')).toBe('pending')
  })

  it('marks every step done once the change is applied', () => {
    expect(stageState('saved', 'applied')).toBe('done')
    expect(stageState('reconnecting', 'applied')).toBe('done')
    expect(stageState('applied', 'applied')).toBe('done')
  })

  /// A failure stops at the step that failed rather than pretending the later
  /// ones happened.
  it('leaves a failure standing on the applying step', () => {
    expect(stageState('saved', 'failed')).toBe('done')
    expect(stageState('applying', 'failed')).toBe('active')
    expect(stageState('applied', 'failed')).toBe('pending')
  })
})
