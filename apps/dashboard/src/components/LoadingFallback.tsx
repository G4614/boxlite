/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { LogoText } from '@/assets/Logo'
import { useEffect, useState } from 'react'
import { Skeleton } from './ui/skeleton'

const LoadingFallback = () => {
  const [showLongLoadingMessage, setShowLongLoadingMessage] = useState(false)

  useEffect(() => {
    const timer = setTimeout(() => {
      setShowLongLoadingMessage(true)
    }, 5_000)

    return () => clearTimeout(timer)
  }, [])

  return (
    <div className="flex min-h-svh flex-col bg-background text-foreground">
      <header className="border-b border-border bg-background">
        <div className="mx-auto flex h-14 w-full max-w-[1040px] items-center gap-3 px-4 sm:px-5 2xl:px-0">
          <div className="shrink-0 text-[1.15rem] font-semibold tracking-tight">
            <LogoText />
          </div>
          <div className="hidden items-center gap-2 sm:flex">
            <Skeleton className="h-5 w-16" />
          </div>
          <div className="ml-auto flex items-center gap-2">
            <Skeleton className="hidden h-8 w-28 md:block" />
            <Skeleton className="h-8 w-8 sm:w-24" />
          </div>
        </div>
      </header>

      <main className="mx-auto w-full max-w-[1040px] flex-1 px-4 pb-8 pt-7 sm:px-5 2xl:px-0">
        <div className="space-y-3">
          <Skeleton className="h-9 w-full max-w-[360px]" />
          <div className="overflow-hidden rounded-sm border border-border">
            <Skeleton className="h-10 rounded-none border-b border-border" />
            <Skeleton className="h-10 rounded-none border-b border-border" />
            <Skeleton className="h-10 rounded-none border-b border-border" />
            <Skeleton className="h-10 rounded-none" />
          </div>
        </div>

        {showLongLoadingMessage && (
          <div className="mt-10 space-y-1 text-center text-sm text-muted-foreground">
            <p>This is taking longer than expected...</p>
            <p>
              If this issue persists, contact us at{' '}
              <a href="mailto:support@boxlite.ai" className="text-primary underline">
                support@boxlite.ai
              </a>
              .
            </p>
          </div>
        )}
      </main>
    </div>
  )
}

export default LoadingFallback
