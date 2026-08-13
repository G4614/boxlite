/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { ApiProperty, ApiSchema } from '@nestjs/swagger'
import { IsOptional, IsString, Validate, ValidatorConstraint, ValidatorConstraintInterface } from 'class-validator'
import { validate as isUuid } from 'uuid'

// Volume ids and names share one id-or-name lookup (VolumeService.findOneByIdOrName,
// validateVolumes): both are matched against the same input string. A name that
// happens to look like a UUID could collide with another volume's real id in the
// same organization, making that lookup ambiguous. Reserving the UUID shape
// entirely for server-assigned ids keeps the two namespaces disjoint by
// construction, rather than only catching collisions that already exist.
@ValidatorConstraint({ name: 'isNotUuidShaped', async: false })
class IsNotUuidShapedConstraint implements ValidatorConstraintInterface {
  validate(value: unknown): boolean {
    return typeof value !== 'string' || !isUuid(value)
  }

  defaultMessage(): string {
    return 'name must not be a UUID - that shape is reserved for server-assigned volume ids'
  }
}

@ApiSchema({ name: 'CreateVolume' })
export class CreateVolumeDto {
  @ApiProperty()
  @IsOptional()
  @IsString()
  @Validate(IsNotUuidShapedConstraint)
  name?: string
}
