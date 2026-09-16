/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

/**
 * Put text on the clipboard and report whether it actually got there.
 *
 * Every copy button in the console was written the same way: call
 * `navigator.clipboard?.writeText(value)` inside a `try/catch`, then say
 * "Copied". `writeText` *rejects*, it does not throw, so the `catch` never
 * ran and a denied permission still reported success. The one case the
 * `try` did cover — no `navigator.clipboard` at all, where the property
 * access throws — is the case the optional chain had already swallowed.
 *
 * Returning a boolean rather than throwing keeps the decision at the call
 * site: a button that reports the wrong thing is the bug being fixed here,
 * so "did it copy" has to be answerable.
 */
export async function copyToClipboard(text: string, clipboard = globalThis.navigator?.clipboard): Promise<boolean> {
  try {
    await clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}
