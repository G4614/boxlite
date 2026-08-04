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

// Validate the portable spelling here, but leave kernel support checks to the
// guest. Linux can add capabilities independently of an API release.
const LINUX_CAPABILITY_NAME = /^[A-Z][A-Z0-9_]*$/

/** Return the canonical, unprefixed capability spelling used on the wire. */
export function canonicalizeLinuxCapability(value: string): string {
  const normalized = value.toUpperCase()
  const name = normalized.startsWith('CAP_') ? normalized.slice(4) : normalized

  if (!LINUX_CAPABILITY_NAME.test(name)) {
    throw new BadRequestError(`Malformed Linux capability '${value}'`)
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
 * and runner adapters. Privileged mode is a separate shape from capability
 * overrides and cannot be combined with them.
 */
export function normalizeBoxAdvancedOptions(input: BoxAdvancedOptionsInput = {}): NormalizedBoxAdvancedOptions {
  if (input.privileged !== undefined && typeof input.privileged !== 'boolean') {
    throw new BadRequestError('privileged must be a boolean')
  }

  const privileged = input.privileged ?? false
  if (privileged) {
    if ((input.capabilities?.add?.length ?? 0) > 0 || (input.capabilities?.drop?.length ?? 0) > 0) {
      throw new BadRequestError('privileged mode cannot be combined with cap_add or cap_drop')
    }

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
