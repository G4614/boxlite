/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BadRequestError } from '../../exceptions/bad-request.exception'
import { normalizeBoxAdvancedOptions } from './advanced-options.util'

describe('normalizeBoxAdvancedOptions', () => {
  it('makes privileged mode authoritative over explicit capabilities', () => {
    expect(
      normalizeBoxAdvancedOptions({
        privileged: true,
        capabilities: { add: ['SYS_ADMIN'], drop: ['NET_RAW'] },
      }),
    ).toEqual({ privileged: true, capabilities: { add: ['ALL'], drop: [] } })
  })

  it('canonicalizes capability spelling and removes semantic duplicates', () => {
    expect(
      normalizeBoxAdvancedOptions({
        capabilities: { add: ['cap_net_admin', 'NET_ADMIN'], drop: ['CAP_NET_RAW'] },
      }),
    ).toEqual({ privileged: false, capabilities: { add: ['NET_ADMIN'], drop: ['NET_RAW'] } })
  })

  it('rejects unknown capability names', () => {
    expect(() => normalizeBoxAdvancedOptions({ capabilities: { add: ['NOT_A_CAPABILITY'] } })).toThrow(BadRequestError)
  })
})
