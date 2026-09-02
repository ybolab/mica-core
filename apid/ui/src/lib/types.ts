export interface TaskAccepted {
  taskId: string
  task?: { id: string; status: string }
}

export interface TaskRecord {
  id: string
  operation: string
  dotPath: string
  source: string
  status: string
  enqueuedAt: string
  startedAt?: string
  finishedAt?: string
  outcome?: string
  message?: string
  foldedCount: number
}

export interface TimeStatus {
  status: 'synchronized' | 'synchronizing' | 'offline-degraded' | 'invalid-source' | 'unknown'
  synchronized?: boolean
  detail?: string
  server?: { name?: string | null; address?: string | null }
  sample?: {
    leap: number
    stratum: number
    spike: boolean
    offsetSeconds: number
    packetCount: number
    correction: 'step' | 'slew'
  }
}

export interface StorageSpace {
  totalBytes: number
  usedBytes: number
  freeBytes: number
  reservedBytes: number
  usedPercent: number
}

export interface StorageTier {
  name: string
  role: string
  partitionLabel: string
  expectedMount?: string
  present: boolean
  detail?: string
  device?: string
  partitionBytes?: number
  mounted?: boolean
  mount?: string
  filesystem?: string
  readOnly?: boolean
  space?: StorageSpace
  pressure?: 'normal' | 'warning' | 'critical'
  updateWorkspace?: { root: string; reservedBytes: number; available: boolean }
  check: {
    recorded?: false
    unit?: string
    activeState?: string | null
    result?: string | null
    exitStatus?: number | null
  }
}

/// Absent wear is reported as unsupported with a reason, never omitted: a
/// medium nobody can read must not look healthy.
export interface StorageMedium {
  name: string
  kind: string
  sizeBytes?: number
  model?: string
  rotational?: boolean
  health:
    | {
        supported: true
        source: string
        raw: { lifeTime: string; preEolInfo?: string | null }
        lifetimeEstimates: { raw: string; usedPercentMin?: number; usedPercentMax?: number; detail?: string }[]
        preEol?: string | null
      }
    | { supported: false; reason: string }
}

/// A PLAN-063 bind namespace of the DATA filesystem. It carries no capacity
/// of its own on purpose: /mos and /srv are two views of one pool, and a
/// second capacity here would invite a reader to add them together.
export interface StorageBind {
  name: string
  mount: string
  source: string
  owner: string
  readiness: 'ready' | 'degraded' | 'unavailable' | 'unknown'
  detail?: string
  mounted?: boolean
  device?: string
  readOnly?: boolean
  sourceOnData?: boolean
  sourceIsDirectory?: boolean
  probe?:
    | { attempted: true; passed: true }
    | { attempted: true; passed: false; error: string }
    | { attempted: false; reason: string }
}

export interface StorageStatus {
  tiers: StorageTier[]
  namespaces: {
    sharedCapacityTier: string
    detail: string
    binds: StorageBind[]
  }
  media: StorageMedium[]
  policy: {
    warningPercent: number
    warningClearPercent: number
    criticalPercent: number
    criticalClearPercent: number
    updateWorkspaceReservedBytes: number
    updateWorkspaceRoot: string
    watchedTiers: string[]
  }
  lifecycle: Record<string, string>
}

export interface Meta {
  api: string
  settingsSchemaVersion: number
  daemon: string
}

export interface Health {
  apid: string
  mosd: string
  checkedAt?: number
  detail?: string
}

export interface ObservedInterface {
  index?: number
  name?: string
  kind?: string
  type?: string
  driver?: string
  administrativeState?: string
  operationalState?: string
  carrierState?: string
  addressState?: string
  ipv4AddressState?: string
  ipv6AddressState?: string
  onlineState?: string
  mtu?: number
  hardwareAddress?: unknown
  addresses?: unknown[]
  dns?: unknown[]
  routes?: unknown[]
}

export interface NetworkOverview {
  configured: Record<string, unknown>
  configuredCount: number
  observed: {
    available: boolean
    interfaceCount: number
    interfaces: ObservedInterface[]
    error?: string
  }
}

export interface UiStatus {
  mode: 'builtIn' | 'custom'
  custom?: CustomUiDetails
  availableCustom?: CustomUiDetails & {
    usable: boolean
    unavailableReason?:
      | 'missingActivationRecord'
      | 'unsafeTree'
      | 'indexUnavailable'
      | 'manifestInvalid'
      | 'digestMismatch'
      | 'incompatible'
  }
}

export interface CustomUiDetails {
  generation: number
  indexReadable: boolean
  name?: string
  version?: string
  digestMatches?: boolean
  compatible?: boolean
}

export interface UiBundleDetails extends CustomUiDetails {
  digest?: string
  compressedBytes?: number
  expandedBytes?: number
  usable: boolean
  unavailableReason?: NonNullable<UiStatus['availableCustom']>['unavailableReason']
}

export interface UiBundleList {
  activeGeneration?: number
  bundles: UiBundleDetails[]
  retentionLimit: number
}
