/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { MigrationInterface, QueryRunner } from 'typeorm'

/**
 * Resync the box table with the post-#735 entity.
 *
 * The squashed 1741087887225-migration.ts baseline matches the new entity
 * shape, but TypeORM tracks migrations by name, so stacks that ran a
 * pre-squash baseline (the live Tokyo e2e stack among them) keep their old
 * schema and stay marked as "applied". Two divergences surface as 500s:
 *
 *   (1) column "image" of relation "box" does not exist
 *       — #735's first-class image column never got ALTER-ed in.
 *   (2) null value in column "boxId" of relation "box" violates not-null
 *       constraint
 *       — pre-#735 Box entity had a separate `boxId` field with a
 *       generated default; #735's 0e6b8758 collapsed it into `id`, so
 *       INSERTs no longer supply `boxId` and the stale NOT NULL constraint
 *       trips.
 *
 * Idempotent both ways: fresh stacks (or anything that already converged
 * via the new baseline) hit the IF-EXISTS / IF-NOT-EXISTS no-op branches.
 */
export class AddImageToBox1749700000000 implements MigrationInterface {
  name = 'AddImageToBox1749700000000'

  async up(queryRunner: QueryRunner): Promise<void> {
    // Belt-and-suspenders: pre-#735/#736 schemas accumulated several
    // box columns the new entity no longer references. Each `DROP COLUMN
    // IF EXISTS` is a no-op on stacks that have already converged.
    const obsoleteBoxColumns = [
      'boxId', // collapsed into id by #735/0e6b8758
      'autoArchiveInterval', // archive flow removed pre-launch
      'backupErrorReason', // backup subsystem deleted in #7ec370b7
      'backupRegistryId',
      'backupSnapshot',
      'snapshot', // snapshot/template subsystem deleted in #7ec370b7
      'snapshotName',
      'template',
      'templateId',
      'artifactRef', // runner artifact handoff deleted in #7ec370b7
    ]

    await queryRunner.query(`
      DO $$
      BEGIN
        IF NOT EXISTS (
          SELECT 1 FROM information_schema.columns
          WHERE table_name = 'box' AND column_name = 'image'
        ) THEN
          ALTER TABLE "box" ADD COLUMN "image" character varying NOT NULL DEFAULT '';
          ALTER TABLE "box" ALTER COLUMN "image" DROP DEFAULT;
        END IF;
      END$$;
    `)

    for (const col of obsoleteBoxColumns) {
      await queryRunner.query(`ALTER TABLE "box" DROP COLUMN IF EXISTS "${col}"`)
    }
  }

  async down(queryRunner: QueryRunner): Promise<void> {
    // No restoration of the collapsed boxId — it's the same value as `id`
    // now, regenerating one would diverge from id and break FKs that ref
    // box.id. Only the image column is reversible.
    await queryRunner.query(`ALTER TABLE "box" DROP COLUMN IF EXISTS "image"`)
  }
}
