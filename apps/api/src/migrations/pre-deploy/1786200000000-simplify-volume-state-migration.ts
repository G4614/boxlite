import { MigrationInterface, QueryRunner } from 'typeorm'

// Collapses volume_state_enum's pending_create/pending_delete stages into
// creating/destroying (the reconciler's per-volume Redis lock already
// distinguishes "queued" from "being processed" — a DB-level pending stage
// was redundant) and renames deleting/deleted to destroying/destroyed to
// match BoxState's vocabulary.
//
// Postgres can't drop enum values, so the old labels (pending_create,
// pending_delete, deleting, deleted) stay defined on volume_state_enum but
// unused — same accepted pattern as the ssh_access table noted in
// migrations/README.md. No rows should carry them after this runs.
export class SimplifyVolumeState1786200000000 implements MigrationInterface {
  name = 'SimplifyVolumeState1786200000000'

  public async up(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.query(`ALTER TYPE "public"."volume_state_enum" ADD VALUE IF NOT EXISTS 'destroying'`)
    await queryRunner.query(`ALTER TYPE "public"."volume_state_enum" ADD VALUE IF NOT EXISTS 'destroyed'`)

    await queryRunner.query(`UPDATE "volume" SET "state" = 'creating' WHERE "state" = 'pending_create'`)
    await queryRunner.query(
      `UPDATE "volume" SET "state" = 'destroying' WHERE "state" IN ('pending_delete', 'deleting')`,
    )
    await queryRunner.query(`UPDATE "volume" SET "state" = 'destroyed' WHERE "state" = 'deleted'`)

    await queryRunner.query(`ALTER TABLE "volume" ALTER COLUMN "state" SET DEFAULT 'creating'`)
  }

  public async down(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.query(`ALTER TABLE "volume" ALTER COLUMN "state" SET DEFAULT 'pending_create'`)
    // Best-effort only: collapsing pending_delete/deleting into destroying lost
    // which of the two a row was originally, so this reverts every destroying
    // row to 'deleting' rather than restoring the exact prior label. 'creating'
    // rows are left alone — it was already a valid state before this migration
    // (a row there could have started as either pending_create or creating),
    // so there's no correct single label to revert it to.
    await queryRunner.query(`UPDATE "volume" SET "state" = 'deleting' WHERE "state" = 'destroying'`)
    await queryRunner.query(`UPDATE "volume" SET "state" = 'deleted' WHERE "state" = 'destroyed'`)
    // 'destroying'/'destroyed' remain on the enum type — Postgres cannot drop
    // enum values without recreating the type, which isn't worth the risk here.
  }
}
