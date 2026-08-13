/*
 * Copyright 2025 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0
 */

import {
  IsNotEmpty,
  IsOptional,
  IsString,
  Validate,
  ValidatorConstraint,
  ValidatorConstraintInterface,
} from 'class-validator'
import { validate as isUuid } from 'uuid'

// Same guard as the classic POST /volumes DTO (apps/api/src/box/dto/create-volume.dto.ts):
// VolumeService.findOneByIdOrName/validateVolumes match a caller's string against
// both the id and name columns in one lookup, so a name shaped like a UUID could
// collide with a different volume's real id in the same organization.
@ValidatorConstraint({ name: 'isNotUuidShaped', async: false })
class IsNotUuidShapedConstraint implements ValidatorConstraintInterface {
  validate(value: unknown): boolean {
    return typeof value !== 'string' || !isUuid(value)
  }

  defaultMessage(): string {
    return 'name must not be a UUID - that shape is reserved for server-assigned volume ids'
  }
}

export class CreateVolumeDto {
  // Optional — VolumeService.create() defaults an absent name to the
  // server-assigned id. IsNotEmpty (not just IsOptional + IsString) rejects
  // an explicit `name: ''` instead of silently falling through to that
  // default.
  @IsOptional()
  @IsString()
  @IsNotEmpty()
  @Validate(IsNotUuidShapedConstraint)
  name?: string
}
