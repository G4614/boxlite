/*
 * Copyright 2025 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BoxState } from '../../box/enums/box-state.enum'
import { boxToBoxResponse, createBoxToCreateBox } from './box-to-box.mapper'

describe('BoxLite lifecycle policy mapper', () => {
  it.each([
    ['enabled', true],
    ['disabled', false],
  ])('maps inbound.mode=%s to control-plane public=%s', (mode, expected) => {
    const mapped = createBoxToCreateBox({
      network: {
        outbound: { mode: 'enabled' },
        inbound: { mode: mode as 'enabled' | 'disabled' },
      },
    })

    expect(mapped.public).toBe(expected)
  })

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

  it('maps REST volume specs to managed volume mounts', () => {
    const mapped = createBoxToCreateBox({
      volumes: [{ volume: 'volume-123', guest_path: '/data', read_only: false }],
    })

    expect(mapped.volumes).toEqual([{ volumeId: 'volume-123', mountPath: '/data' }])
  })

  // host_path is reserved for a future host-filesystem bind mount (its
  // original, pre-managed-volumes meaning on this REST surface) - not a
  // volume alias. A REST box runs on a remote runner, so it isn't
  // implemented; reject its mere presence rather than misreading it as a
  // volume reference or silently ignoring it.
  it('rejects host_path even when a valid volume is also present', () => {
    expect(() =>
      createBoxToCreateBox({
        volumes: [{ volume: 'volume-123', host_path: '/some/path', guest_path: '/data', read_only: false }],
      }),
    ).toThrow('host_path (host-filesystem bind mount) is not supported for remote boxes')
  })

  it('rejects host_path alone (no volume)', () => {
    expect(() =>
      createBoxToCreateBox({
        volumes: [{ host_path: '/some/path', guest_path: '/data', read_only: false }],
      } as any),
    ).toThrow('host_path (host-filesystem bind mount) is not supported for remote boxes')
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
