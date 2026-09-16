// @vitest-environment jsdom
/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { OnboardingGuideDialog } from './OnboardingGuideDialog'

const mocks = vi.hoisted(() => ({
  createApiKey: vi.fn(),
  hasPermission: vi.fn(() => true),
}))

vi.mock('@/hooks/useApi', () => ({
  useApi: () => ({ apiKeyApi: { createApiKey: mocks.createApiKey }, boxApi: { listBoxesPaginated: vi.fn() } }),
}))
vi.mock('@/hooks/useConfig', () => ({
  useConfig: () => ({ apiUrl: 'https://api.test', oidc: { issuer: 'https://auth.test' } }),
}))
vi.mock('@/hooks/useSelectedOrganization', () => ({
  useSelectedOrganization: () => ({
    selectedOrganization: { id: 'org-1', name: 'Org' },
    organizationMembers: [{ id: 'member-1' }],
    refreshOrganizationMembers: vi.fn(),
    authenticatedUserHasPermission: mocks.hasPermission,
  }),
}))
vi.mock('react-oidc-context', () => ({ useAuth: () => ({ user: { profile: { sub: 'user-1' } } }) }))
// The handoff screen polls for boxes; this test is about the other tab.
vi.mock('@tanstack/react-query', () => ({ useQuery: () => ({ data: [] }) }))
vi.mock('sonner', () => ({ toast: { error: vi.fn(), success: vi.fn() } }))

let root: Root | null = null

function flush() {
  return act(async () => {
    await Promise.resolve()
  })
}

function tabNamed(label: string) {
  return [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((t) =>
    (t.textContent ?? '').includes(label),
  )
}

// Radix activates a tab on `mousedown`, so a bare click never switched panels.
async function click(el: HTMLElement | undefined) {
  await act(async () => {
    el?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, button: 0 }))
    el?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
  })
  await flush()
}

beforeEach(() => {
  mocks.createApiKey.mockResolvedValue({ data: { name: 'sdk-quickstart', value: 'blk_live_theOnlyCopy' } })
})

afterEach(() => {
  act(() => root?.unmount())
  root = null
  document.body.innerHTML = ''
  vi.clearAllMocks()
})

describe('OnboardingGuideDialog tabs', () => {
  it('keeps a created key when the user looks at the other job and comes back', async () => {
    // The plaintext key is returned exactly once. Changing tabs used to reset
    // the walkthrough, so this round trip discarded it for good and left a
    // live key in the account that the user never saw.
    const host = document.createElement('div')
    document.body.appendChild(host)
    await act(async () => {
      root = createRoot(host)
      root.render(<OnboardingGuideDialog open onOpenChange={() => {}} onProgressChange={() => {}} />)
    })
    await flush()

    await click(tabNamed('Run untrusted code'))
    const createKey = [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (b) => (b.textContent ?? '').trim() === 'Create key',
    )
    await click(createKey)
    expect(document.body.textContent).toContain('blk_live_theOnlyCopy')

    await click(tabNamed('Build an app online'))
    await click(tabNamed('Run untrusted code'))

    expect(document.body.textContent).toContain('blk_live_theOnlyCopy')
    expect(mocks.createApiKey).toHaveBeenCalledTimes(1)
  })
})
