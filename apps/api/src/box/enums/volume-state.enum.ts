/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

// Mirrors BoxState's naming (creating/destroying/destroyed/error) so the two
// resources read consistently. No separate pending_* stage: a volume moves
// straight to CREATING/DESTROYING on request, same as a box does — the
// reconciler's per-volume Redis lock (see VolumeManager) already distinguishes
// "queued" from "being processed", so a DB-level pending stage was redundant.
export enum VolumeState {
  CREATING = 'creating',
  READY = 'ready',
  DESTROYING = 'destroying',
  DESTROYED = 'destroyed',
  ERROR = 'error',
}
