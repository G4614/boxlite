/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { cn } from '@/lib/utils'

// Shared by every copyable block in Quickstart, so a new one cannot drift into
// its own button style.
export function QuickstartCopyButton({
  copied,
  failed,
  onClick,
  className,
}: {
  copied: boolean
  /** A denied clipboard permission used to look exactly like an unclicked
   *  button. It is the one moment this screen cannot afford to be silent. */
  failed?: boolean
  onClick: () => void
  className?: string
}) {
  const accent = failed ? 'destructive' : 'success'
  return (
    <button
      type="button"
      onClick={onClick}
      style={{
        boxShadow: copied || failed ? `3px 3px 0 hsl(var(--${accent}) / 0.35)` : '3px 3px 0 hsl(var(--border))',
      }}
      className={cn(
        'flex h-7 min-w-[76px] flex-none items-center justify-center border-2 bg-[hsl(var(--code-background))] px-[10px] text-[10px] font-semibold uppercase tracking-[1px] transition-[color,border-color,background-color,transform,box-shadow] active:translate-x-px active:translate-y-px active:shadow-none',
        failed
          ? 'border-destructive bg-[hsl(var(--destructive)/0.14)] text-destructive'
          : copied
            ? 'border-success bg-[hsl(var(--success)/0.14)] text-success'
            : 'border-border text-muted-foreground hover:border-brand hover:text-foreground',
        className,
      )}
    >
      {failed ? '✕ Blocked' : copied ? '✓ Copied' : 'Copy'}
    </button>
  )
}
