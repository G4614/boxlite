/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { MigrationInterface, QueryRunner } from 'typeorm'

/**
 * Add `image` column to box.
 *
 * #735 added `image` to the squashed baseline 1741087887225-migration.ts so
 * fresh stacks create the column at first boot. TypeORM tracks migrations
 * by name though, so any stack that ran the baseline before the squash —
 * including the live Tokyo e2e stack — has the migration row marked as
 * applied with a schema that predates the `image` column. SELECT/INSERT
 * from the rewritten Box entity then fails with
 *   column "image" of relation "box" does not exist
 * surfacing in the API as a 500 on every box request.
 *
 * Idempotent: only ALTER when the column is missing, so stacks that
 * already created it via the new baseline are no-ops.
 */
export class AddImageToBox1749700000000 implements MigrationInterface {
  name = 'AddImageToBox1749700000000'

  async up(queryRunner: QueryRunner): Promise<void> {
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
  }

  async down(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.query(`ALTER TABLE "box" DROP COLUMN IF EXISTS "image"`)
  }
}
