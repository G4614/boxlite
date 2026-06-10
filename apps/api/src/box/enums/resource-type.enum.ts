/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

export enum ResourceType {
  // PR #706 zero-DDL: DB enum `job_resourcetype_enum` still holds the
  // literal `'SANDBOX'`. Keep the TS member name as `BOX` (in line
  // with the entity rename) but pin the wire/DB literal to
  // `'SANDBOX'` so existing rows round-trip cleanly.
  BOX = 'SANDBOX',
  SNAPSHOT = 'SNAPSHOT',
  BACKUP = 'BACKUP',
}
