// @vitest-environment jsdom
/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { beforeEach, describe, expect, it } from 'vitest'
import { HANDOFF_LIFETIME_MS, handoffStore, shouldIssueKey, shouldRestoreCompletion } from './quickstart-handoff'

const SECRET = 'blk_live_notARealKeyJustATestOne'

function storageContents() {
  const dump = (store: Storage) =>
    Array.from({ length: store.length }, (_, i) => {
      const key = store.key(i) ?? ''
      return `${key}=${store.getItem(key) ?? ''}`
    }).join('\n')
  return `${dump(window.sessionStorage)}\n${dump(window.localStorage)}`
}

describe('handoffStore', () => {
  beforeEach(() => {
    window.sessionStorage.clear()
    window.localStorage.clear()
  })

  it('keeps the minted key out of browser storage', () => {
    // The key is a bearer credential. Anything that reaches the document can
    // read `sessionStorage`, so the store holds it in memory for the life of
    // the tab and writes nothing.
    handoffStore.set('user-1_org-1', { keyValue: SECRET, keyName: 'quickstart', baselineIds: [], issuedAt: Date.now() })

    expect(handoffStore.get('user-1_org-1')?.keyValue).toBe(SECRET)
    expect(storageContents()).not.toContain(SECRET)
    expect(storageContents()).not.toContain('quickstart')
  })

  it('drops a handoff once the key it holds has expired', () => {
    // The store used to be `sessionStorage`, whose read enforced this. A tab
    // left open past the key's life would otherwise offer a dead credential
    // and never remint.
    const issuedAt = Date.now()
    handoffStore.set('user-3_org-3', { keyValue: SECRET, keyName: 'quickstart', baselineIds: [], issuedAt })

    expect(handoffStore.get('user-3_org-3', issuedAt + HANDOFF_LIFETIME_MS - 1)?.keyValue).toBe(SECRET)
    expect(handoffStore.get('user-3_org-3', issuedAt + HANDOFF_LIFETIME_MS + 1)).toBeNull()
  })

  it('never hands one identity the key minted for another', () => {
    handoffStore.set('user-1_org-1', { keyValue: SECRET, keyName: 'quickstart', baselineIds: [], issuedAt: Date.now() })

    expect(handoffStore.get('user-1_org-2')).toBeNull()
    expect(handoffStore.get('user-2_org-1')).toBeNull()
  })
})

describe('shouldRestoreCompletion', () => {
  it('restores again when the identity changes', () => {
    // The bug this replaces: restoration was a single boolean, so the second
    // organization never got its completion restored, and its finished box was
    // already in `baselineIds` — polling could not rediscover it either.
    expect(shouldRestoreCompletion({ hydrated: true, restoredFor: 'user-1_org-1', identity: 'user-1_org-2' })).toBe(
      true,
    )
  })

  it('does not restore twice for the same identity', () => {
    expect(shouldRestoreCompletion({ hydrated: true, restoredFor: 'user-1_org-1', identity: 'user-1_org-1' })).toBe(
      false,
    )
  })

  it('waits until the handoff has been read for the identity on screen', () => {
    expect(shouldRestoreCompletion({ hydrated: false, restoredFor: null, identity: 'user-1_org-1' })).toBe(false)
  })
})

describe('shouldIssueKey', () => {
  const ready = {
    hydrated: true,
    hasHandoff: false,
    failed: false,
    hasOrg: true,
    identity: 'user_orgA',
    permissionCount: 1,
    alreadyIssuingForKey: false,
  }

  it('issues once everything is known and nothing is stored', () => {
    expect(shouldIssueKey(ready)).toBe(true)
  })

  it('refuses while the handoff still belongs to the previous identity', () => {
    // The org-switch mint: the component has re-rendered under org B but its
    // handoff is still org A's, so an unguarded `handoff === null` reads as
    // "org B has nothing stored" and a second live key gets minted.
    expect(shouldIssueKey({ ...ready, hydrated: false, identity: 'user_orgB' })).toBe(false)
  })

  it('refuses when this identity already has a key', () => {
    expect(shouldIssueKey({ ...ready, hasHandoff: true })).toBe(false)
  })

  it('refuses while a request for this identity is already open', () => {
    expect(shouldIssueKey({ ...ready, alreadyIssuingForKey: true })).toBe(false)
  })

  it('refuses after a failure, so retry is the only way back', () => {
    expect(shouldIssueKey({ ...ready, failed: true })).toBe(false)
  })

  it('refuses before permissions or the organization are known', () => {
    expect(shouldIssueKey({ ...ready, permissionCount: 0 })).toBe(false)
    expect(shouldIssueKey({ ...ready, hasOrg: false })).toBe(false)
    expect(shouldIssueKey({ ...ready, identity: null })).toBe(false)
  })
})
