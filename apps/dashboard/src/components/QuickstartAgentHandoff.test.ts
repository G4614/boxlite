/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { describe, expect, it } from 'vitest'
import { findAgentBox, maskKey } from './QuickstartAgentHandoff'

describe('maskKey', () => {
  it('keeps the prefix and hides every character of the body', () => {
    expect(maskKey('blk_live_9fPa1UwT1jRqAbCdEf')).toBe(`blk_live_${'•'.repeat(18)}`)
  })

  it('hides the body of a short prefix too', () => {
    // The prefix length is deployment-configured, so masking from a fixed
    // offset printed real key characters for anything shorter than the one it
    // was written against.
    const masked = maskKey('bx_live_SECRETBODY123')
    expect(masked).toBe(`bx_live_${'•'.repeat(18)}`)
    expect(masked).not.toContain('S')
  })

  it('hides everything when there is no prefix to keep', () => {
    expect(maskKey('SECRETBODY123')).toBe('•'.repeat(18))
  })

  it('never lets a key body reach the mask', () => {
    const body = 'abcdefghijklmnop'
    for (const key of [`blk_live_${body}`, `bx_live_${body}`, `a_${body}`, body]) {
      expect(maskKey(key)).not.toContain(body.slice(0, 4))
    }
  })
})

describe('findAgentBox', () => {
  it('picks the box that was not in the fleet when the key was handed over', () => {
    const baseline = new Set(['box-old-1', 'box-old-2'])
    const boxes = [{ id: 'box-old-1' }, { id: 'box-agent', name: 'agile-otter' }, { id: 'box-old-2' }]

    expect(findAgentBox(boxes, baseline)?.id).toBe('box-agent')
  })

  it('finds nothing while the fleet is unchanged', () => {
    const baseline = new Set(['box-old-1', 'box-old-2'])

    expect(findAgentBox([{ id: 'box-old-1' }, { id: 'box-old-2' }], baseline)).toBeNull()
  })

  it('claims no box before the baseline exists', () => {
    // Guards the window between opening the flow and creating the key: with no
    // baseline every existing box would otherwise look like the agent's.
    expect(findAgentBox([{ id: 'box-old-1' }], null)).toBeNull()
  })

  it('claims no box before the first poll returns', () => {
    expect(findAgentBox(undefined, new Set(['box-old-1']))).toBeNull()
  })

  it('attributes the box regardless of how the clocks compare', () => {
    // The regression this replaced: attribution compared the server's createdAt
    // against the browser's Date.now(). A client clock running ahead of the
    // server left the agent's box permanently unattributed and the flow stuck
    // on "waiting". Identity does not depend on either clock.
    const baseline = new Set(['box-old-1'])
    const agentBoxStampedInThePast = { id: 'box-agent', createdAt: '1999-01-01T00:00:00.000Z' }

    expect(findAgentBox([{ id: 'box-old-1' }, agentBoxStampedInThePast], baseline)?.id).toBe('box-agent')
  })
})
