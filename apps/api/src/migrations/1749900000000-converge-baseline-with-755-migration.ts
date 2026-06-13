/*
 * Copyright 2026 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0
 */

import { MigrationInterface, QueryRunner } from 'typeorm'

/**
 * Converge a stack whose `Migration1741087887225` ledger row predates
 * #755 (or that originally ran the pre-squash migration train and
 * landed in a savedImage / no-image-on-box / no-boxId state).
 *
 * #755 edited the squashed baseline file in-place to:
 *   - rename `warm_pool.savedImage` → `warm_pool.image`
 *   - add `box.image` (+ `box_image_idx`)
 * TypeORM keys the migration ledger by class name, so any stack that
 * recorded `Migration1741087887225` before #755 will *not* re-run it
 * and keeps the old shape. The Tokyo e2e stack is the canonical victim:
 *   `column WarmPool.image does not exist`
 *   `column Box.boxId does not exist`
 *
 * Box.boxId in particular is reported missing because the pre-#736
 * migration train (e.g. 0e6b8758 "collapse the dual uuid/boxId
 * identity") dropped that column before the squash ever introduced it.
 *
 * Every step is wrapped in an information_schema guard so the file is
 * idempotent on fresh stacks (and on stacks that converged via any
 * other path).
 */
export class ConvergeBaselineWith7551749900000000 implements MigrationInterface {
  name = 'ConvergeBaselineWith7551749900000000'

  async up(queryRunner: QueryRunner): Promise<void> {
    // warm_pool: rename savedImage → image if the legacy column is still
    // there, otherwise add image fresh.
    await queryRunner.query(`
      DO $$
      BEGIN
        IF EXISTS (
          SELECT 1 FROM information_schema.columns
          WHERE table_name='warm_pool' AND column_name='savedImage'
        ) AND NOT EXISTS (
          SELECT 1 FROM information_schema.columns
          WHERE table_name='warm_pool' AND column_name='image'
        ) THEN
          ALTER TABLE "warm_pool" RENAME COLUMN "savedImage" TO "image";
        END IF;
      END$$;
    `)
    await queryRunner.query(`
      DO $$
      BEGIN
        IF NOT EXISTS (
          SELECT 1 FROM information_schema.columns
          WHERE table_name='warm_pool' AND column_name='image'
        ) THEN
          ALTER TABLE "warm_pool" ADD COLUMN "image" character varying NOT NULL DEFAULT '';
          ALTER TABLE "warm_pool" ALTER COLUMN "image" DROP DEFAULT;
        END IF;
      END$$;
    `)

    // box.image: nullable, plus the read-path index #755 added.
    await queryRunner.query(`
      DO $$
      BEGIN
        IF NOT EXISTS (
          SELECT 1 FROM information_schema.columns
          WHERE table_name='box' AND column_name='image'
        ) THEN
          ALTER TABLE "box" ADD COLUMN "image" character varying;
        END IF;
      END$$;
    `)
    await queryRunner.query(`CREATE INDEX IF NOT EXISTS "box_image_idx" ON "box" ("image")`)

    // box.boxId: NOT NULL, length 12, app-supplied by generateBoxId().
    // Backfill any existing rows with a 12-hex slice so SET NOT NULL holds.
    await queryRunner.query(`
      DO $$
      BEGIN
        IF NOT EXISTS (
          SELECT 1 FROM information_schema.columns
          WHERE table_name='box' AND column_name='boxId'
        ) THEN
          ALTER TABLE "box" ADD COLUMN "boxId" character varying(12);
          UPDATE "box"
             SET "boxId" = substr(replace(uuid_generate_v4()::text, '-', ''), 1, 12)
           WHERE "boxId" IS NULL;
          ALTER TABLE "box" ALTER COLUMN "boxId" SET NOT NULL;
        END IF;
      END$$;
    `)
    await queryRunner.query(
      `CREATE UNIQUE INDEX IF NOT EXISTS "box_boxid_unique_idx" ON "box" ("boxId")`,
    )
    await queryRunner.query(
      `CREATE INDEX IF NOT EXISTS "box_organizationid_boxid_idx" ON "box" ("organizationId", "boxId")`,
    )
  }

  async down(_queryRunner: QueryRunner): Promise<void> {
    // No-op: rolling back would lose data (boxId backfill is non-reversible,
    // image columns hold the only image reference for live rows post-#755).
  }
}
