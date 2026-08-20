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

  // host_path is a deprecated alias for volume, kept for backward
  // compatibility with existing /v1 clients built against the pre-rename
  // `source` field. It must still carry the `volume://<id>` scheme.
  it('resolves a deprecated host_path="volume://<id>" alias to the bare id', () => {
    const mapped = createBoxToCreateBox({
      volumes: [{ host_path: 'volume://volume-123', guest_path: '/data', read_only: false }] as any,
    })

    expect(mapped.volumes).toEqual([{ volumeId: 'volume-123', mountPath: '/data' }])
  })

  it('rejects a bare host_path without the volume:// scheme (genuine host-filesystem bind mount, not implemented)', () => {
    expect(() =>
      createBoxToCreateBox({
        volumes: [{ host_path: '/some/path', guest_path: '/data', read_only: false }] as any,
      }),
    ).toThrow('host_path must use the volume://<id> scheme')
  })

  it('rejects volume and host_path both present (ambiguous, no precedence to guess)', () => {
    expect(() =>
      createBoxToCreateBox({
        volumes: [
          { volume: 'volume-123', host_path: 'volume://volume-123', guest_path: '/data', read_only: false },
        ] as any,
      }),
    ).toThrow('must not set both volume and the deprecated host_path')
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
