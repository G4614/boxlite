/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { describe, expect, it, vi } from 'vitest'
import { copyToClipboard } from './copy-text'

describe('copyToClipboard', () => {
  it('reports success only after the write resolves', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)

    await expect(copyToClipboard('blk_live_secret', { writeText } as unknown as Clipboard)).resolves.toBe(true)
    expect(writeText).toHaveBeenCalledWith('blk_live_secret')
  })

  it('reports failure when the clipboard rejects', async () => {
    // `writeText` rejects on a denied permission rather than throwing, which
    // is what a surrounding `try/catch` missed — the button said "Copied"
    // for a copy that never happened.
    const writeText = vi.fn().mockRejectedValue(new DOMException('Write permission denied.', 'NotAllowedError'))

    await expect(copyToClipboard('blk_live_secret', { writeText } as unknown as Clipboard)).resolves.toBe(false)
  })

  it('reports failure when there is no clipboard at all', async () => {
    await expect(copyToClipboard('blk_live_secret', undefined)).resolves.toBe(false)
  })
})
