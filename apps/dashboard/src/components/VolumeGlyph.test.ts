/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { VolumeState } from '@boxlite-ai/api-client'
import { describe, expect, it } from 'vitest'
import { volumeGlyphState } from './VolumeGlyph'

describe('volumeGlyphState', () => {
  it('maps each lifecycle state to a drawing', () => {
    expect(volumeGlyphState(VolumeState.READY)).toBe('ready')
    expect(volumeGlyphState(VolumeState.ERROR)).toBe('error')
    expect(volumeGlyphState(VolumeState.PENDING_DELETE)).toBe('leaving')
    expect(volumeGlyphState(VolumeState.DELETING)).toBe('leaving')
    expect(volumeGlyphState(VolumeState.DELETED)).toBe('leaving')
  })

  it('draws anything not yet ready as still forming', () => {
    expect(volumeGlyphState(VolumeState.CREATING)).toBe('creating')
    expect(volumeGlyphState(VolumeState.PENDING_CREATE)).toBe('creating')
    expect(volumeGlyphState(undefined)).toBe('creating')
  })
})
