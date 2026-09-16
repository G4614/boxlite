/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BoxState } from '@boxlite-ai/api-client'
import { describe, expect, it } from 'vitest'
import { boxCubeState, boxShape } from './box-shape'

describe('boxShape', () => {
  it("treats the API's disabled value (0) and an unset autoDelete as a kept box", () => {
    // 0 is AUTO_DELETE_DISABLED, the column default, and what "Never" sends.
    expect(boxShape({ public: false, autoDelete: 0 })).toBe('persistent')
    expect(boxShape({ public: false, autoDelete: undefined })).toBe('persistent')
  })

  it('calls a box disposable only when it will delete itself', () => {
    expect(boxShape({ public: false, autoDelete: 30 })).toBe('disposable')
  })

  it('lets public win over lifecycle', () => {
    expect(boxShape({ public: true, autoDelete: 30 })).toBe('public')
  })
})

describe('boxCubeState', () => {
  it('lets a terminal state override the shape', () => {
    expect(boxCubeState({ public: true, autoDelete: 0, state: BoxState.ERROR })).toBe('error')
    expect(boxCubeState({ public: false, autoDelete: 30, state: BoxState.STOPPED })).toBe('stopped')
  })

  it('falls through to the shape while the box runs', () => {
    expect(boxCubeState({ public: true, autoDelete: 0, state: BoxState.STARTED })).toBe('public')
  })
})
