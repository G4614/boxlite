import { MigrationInterface, QueryRunner } from 'typeorm'

/**
 * Repairs environments where `AddBoxLifecycleSeconds1784250000000` already
 * executed before it was amended (#1135) to rename straight to `autoStop`.
 * TypeORM tracks migrations by class name, so an environment that ran the
 * earlier revision keeps whatever intermediate column name that revision
 * produced (`autoPause`, or the older `autoStopInterval`) and never
 * re-runs the amended version — the code has expected `autoStop` since
 * #1135 landed, so any of those environments fail closed with
 * "column ... does not exist" on every box operation.
 *
 * Idempotent and safe on a fresh database: if `autoStop` already exists,
 * both branches below are skipped.
 */
export class FixBoxAutoStopColumnDrift1785900000000 implements MigrationInterface {
  name = 'FixBoxAutoStopColumnDrift1785900000000'

  private async hasColumn(queryRunner: QueryRunner, column: string): Promise<boolean> {
    const rows = await queryRunner.query(
      `SELECT 1 FROM information_schema.columns WHERE table_name = 'box' AND column_name = $1`,
      [column],
    )
    return rows.length > 0
  }

  public async up(queryRunner: QueryRunner): Promise<void> {
    if (await this.hasColumn(queryRunner, 'autoPause')) {
      await queryRunner.query(`ALTER TABLE "box" RENAME COLUMN "autoPause" TO "autoStop"`)
    } else if (await this.hasColumn(queryRunner, 'autoStopInterval')) {
      await queryRunner.query(`ALTER TABLE "box" RENAME COLUMN "autoStopInterval" TO "autoStop"`)
    }

    await queryRunner.query(`ALTER TABLE "box" DROP CONSTRAINT IF EXISTS "box_auto_pause_interval_nonnegative"`)
    await queryRunner.query(`ALTER TABLE "box" DROP CONSTRAINT IF EXISTS "box_auto_stop_interval_nonnegative"`)
    await queryRunner.query(
      `ALTER TABLE "box" ADD CONSTRAINT "box_auto_stop_interval_nonnegative" CHECK ("autoStop" >= 0)`,
    )
  }

  public async down(): Promise<void> {
    // Pure drift repair — there is no single prior name to roll back to
    // (different environments drifted to different names), and a
    // consistent database never needs this migration reverted.
  }
}
