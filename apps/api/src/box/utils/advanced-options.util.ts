/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BadRequestError } from '../../exceptions/bad-request.exception'
import type { BoxCapabilities } from '../dto/box.dto'

export type BoxAdvancedOptionsInput = {
  privileged?: boolean
  capabilities?: Partial<BoxCapabilities>
}

export type NormalizedBoxAdvancedOptions = {
  privileged: boolean
  capabilities: BoxCapabilities
}

// Keep this list aligned with the Linux capability table used by the guest.
const LINUX_CAPABILITY_NAMES = new Set([
  'CHOWN',
  'DAC_OVERRIDE',
  'DAC_READ_SEARCH',
  'FOWNER',
  'FSETID',
  'KILL',
  'SETGID',
  'SETUID',
  'SETPCAP',
  'LINUX_IMMUTABLE',
  'NET_BIND_SERVICE',
  'NET_BROADCAST',
  'NET_ADMIN',
  'NET_RAW',
  'IPC_LOCK',
  'IPC_OWNER',
  'SYS_MODULE',
  'SYS_RAWIO',
  'SYS_CHROOT',
  'SYS_PTRACE',
  'SYS_PACCT',
  'SYS_ADMIN',
  'SYS_BOOT',
  'SYS_NICE',
  'SYS_RESOURCE',
  'SYS_TIME',
  'SYS_TTY_CONFIG',
  'MKNOD',
  'LEASE',
  'AUDIT_WRITE',
  'AUDIT_CONTROL',
  'SETFCAP',
  'MAC_OVERRIDE',
  'MAC_ADMIN',
  'SYSLOG',
  'WAKE_ALARM',
  'BLOCK_SUSPEND',
  'AUDIT_READ',
  'PERFMON',
  'BPF',
  'CHECKPOINT_RESTORE',
])

/** Return the canonical, unprefixed capability spelling used on the wire. */
export function canonicalizeLinuxCapability(value: string): string {
  const normalized = value.toUpperCase()
  const name = normalized.startsWith('CAP_') ? normalized.slice(4) : normalized

  if (name !== 'ALL' && !LINUX_CAPABILITY_NAMES.has(name)) {
    throw new BadRequestError(`Unknown Linux capability '${value}'`)
  }

  return name
}

export function isValidLinuxCapabilityName(value: unknown): boolean {
  if (typeof value !== 'string') {
    return false
  }

  try {
    canonicalizeLinuxCapability(value)
    return true
  } catch {
    return false
  }
}

function normalizeCapabilityList(values: string[] | undefined): string[] {
  return [...new Set((values ?? []).map(canonicalizeLinuxCapability))]
}

/**
 * Normalize the one public advanced-options contract used by REST, storage,
 * and runner adapters. Privileged mode is authoritative over explicit caps.
 */
export function normalizeBoxAdvancedOptions(input: BoxAdvancedOptionsInput = {}): NormalizedBoxAdvancedOptions {
  if (input.privileged !== undefined && typeof input.privileged !== 'boolean') {
    throw new BadRequestError('privileged must be a boolean')
  }

  const privileged = input.privileged ?? false
  if (privileged) {
    return {
      privileged: true,
      capabilities: { add: ['ALL'], drop: [] },
    }
  }

  return {
    privileged: false,
    capabilities: {
      add: normalizeCapabilityList(input.capabilities?.add),
      drop: normalizeCapabilityList(input.capabilities?.drop),
    },
  }
}
