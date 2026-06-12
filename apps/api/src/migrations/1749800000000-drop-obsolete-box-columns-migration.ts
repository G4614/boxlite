/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { MigrationInterface, QueryRunner } from 'typeorm'

/**
 * Drop the obsolete box columns left over from pre-#735 schemas.
 *
 * 1749700000000-add-image-to-box-migration was first deployed with only
 * the `image` ADD step; TypeORM's migration ledger keys by class name,
 * so adding the DROP-COLUMN sweep into the same file silently
 * no-ops on stacks that already ran the first version (Tokyo e2e
 * included). This file is the second pass — same idempotent DROP
 * COLUMN IF EXISTS sweep but as a fresh migration class so the ledger
 * picks it up.
 *
 * Columns removed:
 *   boxId            collapsed into id (#735/0e6b8758)
 *   autoArchiveInterval  archive flow removed pre-launch
 *   backupErrorReason    backup subsystem deleted (#7ec370b7)
 *   backupRegistryId
 *   backupSnapshot
 *   snapshot         snapshot/template subsystem deleted (#7ec370b7)
 *   snapshotName
 *   template
 *   templateId
 *   artifactRef      runner artifact handoff deleted (#7ec370b7)
 */
export class DropObsoleteBoxColumns1749800000000 implements MigrationInterface {
  name = 'DropObsoleteBoxColumns1749800000000'

  async up(queryRunner: QueryRunner): Promise<void> {
    const cols = [
      'boxId',
      'autoArchiveInterval',
      'backupErrorReason',
      'backupRegistryId',
      'backupSnapshot',
      'snapshot',
      'snapshotName',
      'template',
      'templateId',
      'artifactRef',
    ]
    for (const col of cols) {
      await queryRunner.query(`ALTER TABLE "box" DROP COLUMN IF EXISTS "${col}"`)
    }
  }

  async down(_queryRunner: QueryRunner): Promise<void> {
    // No-op: regenerating the dropped columns would require the original
    // generators/defaults which were deleted with the parent subsystems.
  }
}
