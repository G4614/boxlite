/*
 * Copyright 2025 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BoxState } from '../../box/enums/box-state.enum'
import { boxToBoxResponse, createBoxToCreateBox } from './box-to-box.mapper'

describe('BoxLite lifecycle policy mapper', () => {
  it('maps second-based create fields into the control-plane DTO', () => {
    const mapped = createBoxToCreateBox({
      auto_stop: 1800,
      auto_delete: 604800,
      auto_resume: false,
    })

    expect(mapped.autoStop).toBe(1800)
    expect(mapped.autoDelete).toBe(604800)
    expect(mapped.autoResume).toBe(false)
  })

  it('maps advanced privileged options into the control-plane DTO', () => {
    const mapped = createBoxToCreateBox({ advanced: { privileged: true } })

    expect(mapped.privileged).toBe(true)
    expect(mapped.capabilities).toEqual({ add: ['ALL'], drop: [] })
  })

  it('rejects privileged capability overrides at the REST boundary', () => {
    expect(() =>
      createBoxToCreateBox({
        advanced: {
          privileged: true,
          capabilities: { add: ['SYS_ADMIN'], drop: ['NET_RAW'] },
        },
      }),
    ).toThrow('cannot be combined')
  })

  it('canonicalizes capability names when privileged mode is disabled', () => {
    const mapped = createBoxToCreateBox({
      advanced: {
        capabilities: { add: ['cap_net_admin', 'NET_ADMIN'], drop: ['CAP_NET_RAW'] },
      },
    })

    expect(mapped.privileged).toBe(false)
    expect(mapped.capabilities).toEqual({ add: ['NET_ADMIN'], drop: ['NET_RAW'] })
  })

  it('returns the effective second-based policy', () => {
    const response = boxToBoxResponse({
      id: 'box-1',
      name: 'demo',
      state: BoxState.STARTED,
      labels: {},
      autoStop: 1800,
      autoDelete: 604800,
      autoResume: false,
    } as any)

    expect(response.auto_stop).toBe(1800)
    expect(response.auto_delete).toBe(604800)
    expect(response.auto_resume).toBe(false)
  })

  it('defaults auto_resume to true when missing', () => {
    const response = boxToBoxResponse({
      id: 'box-1',
      name: 'demo',
      state: BoxState.STARTED,
      labels: {},
    } as any)

    expect(response.auto_resume).toBe(true)
  })
})
