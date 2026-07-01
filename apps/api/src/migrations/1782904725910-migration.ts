import { MigrationInterface, QueryRunner } from 'typeorm'

export class Migration1782904725910 implements MigrationInterface {
  name = 'Migration1782904725910'

  public async up(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.query(`ALTER TABLE "box" ADD "ports" jsonb NOT NULL DEFAULT '[]'`)
  }

  public async down(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.query(`ALTER TABLE "box" DROP COLUMN "ports"`)
  }
}
