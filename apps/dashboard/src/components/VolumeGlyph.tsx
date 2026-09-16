/*
 * Modified by BoxLite AI, 2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { cn } from '@/lib/utils'
import { VolumeState } from '@boxlite-ai/api-client'

/**
 * The volume, drawn in the same register as `BoxCube`: one isometric wireframe,
 * single stroke, `currentColor`. A storage volume is a cylinder — the shape
 * every disk and bucket has been drawn as — laid flat like the foundation a
 * persistent box stands on, because that is what it is to a box.
 */
export type VolumeGlyphState = 'ready' | 'creating' | 'error' | 'leaving'

export function volumeGlyphState(state: VolumeState | undefined): VolumeGlyphState {
  switch (state) {
    case VolumeState.READY:
      return 'ready'
    case VolumeState.ERROR:
      return 'error'
    case VolumeState.PENDING_DELETE:
    case VolumeState.DELETING:
    case VolumeState.DELETED:
      return 'leaving'
    default:
      return 'creating'
  }
}

// Top ellipse centred at (32,22); the body drops 20px; the bottom is the
// visible front half of the same ellipse.
const TOP = 'M10 22a22 11 0 1 0 44 0a22 11 0 1 0 -44 0Z'
const SIDES = 'M10 22v20M54 22v20'
const BOTTOM = 'M10 42a22 11 0 0 0 44 0'

export function VolumeGlyph({
  state,
  size = 64,
  className,
  title,
}: {
  state: VolumeGlyphState
  size?: number
  className?: string
  title?: string
}) {
  const detailed = size >= 28
  const stroke = { fill: 'none', stroke: 'currentColor', strokeWidth: 1.5 } as const
  const dashed = state === 'creating' ? { strokeDasharray: '3 3' } : {}

  return (
    <svg
      viewBox="0 0 64 64"
      width={size}
      height={size}
      className={cn('shrink-0', state === 'leaving' && 'opacity-50', className)}
      role={title ? 'img' : undefined}
      aria-label={title}
      aria-hidden={title ? undefined : true}
    >
      <path d={TOP} {...stroke} {...dashed} />
      {state === 'error' ? (
        // one wall is broken: the volume exists, its integrity does not
        <>
          <path d="M10 22v8M10 36v6M54 22v20" {...stroke} />
          {detailed && <path d="M7 30l6 6" stroke="hsl(var(--destructive))" strokeWidth={2} />}
        </>
      ) : (
        <path d={SIDES} {...stroke} {...dashed} />
      )}
      <path d={BOTTOM} {...stroke} {...dashed} />
      {/* a second rim below the top, and on the top the same brand mark a
          running box carries: the data is here */}
      {detailed && state === 'ready' && (
        <>
          <path d="M10 22a22 11 0 0 0 44 0" {...stroke} opacity={0.45} transform="translate(0 4)" />
          <ellipse cx="32" cy="22" rx="4" ry="2.2" fill="hsl(var(--brand))" />
        </>
      )}
    </svg>
  )
}
