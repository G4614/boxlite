/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { Type } from 'class-transformer'
import {
  ArrayMaxSize,
  IsOptional,
  IsString,
  IsNumber,
  IsBoolean,
  IsObject,
  IsArray,
  Min,
  IsIn,
  Validate,
  ValidateIf,
  ValidateNested,
  ValidationArguments,
  ValidatorConstraint,
  ValidatorConstraintInterface,
} from 'class-validator'
import { isValidNetworkAllowEntry, MAX_NETWORK_ALLOW_LIST_ENTRIES } from '../../box/utils/network-validation.util'

@ValidatorConstraint({ name: 'isNetworkAllowEntry', async: false })
class IsNetworkAllowEntryConstraint implements ValidatorConstraintInterface {
  validate(value: unknown): boolean {
    return typeof value === 'string' && isValidNetworkAllowEntry(value)
  }

  defaultMessage(): string {
    return 'each allow_net entry must be an IPv4 address, IPv4 CIDR, hostname, or wildcard hostname'
  }
}

// Attached to `guest_path` (always validated) rather than `source`, since
// `@IsOptional()` on `source` would skip a validator stacked on that same
// property whenever `source` is absent - exactly the case this needs to see.
@ValidatorConstraint({ name: 'hasVolumeSource', async: false })
class HasVolumeSourceConstraint implements ValidatorConstraintInterface {
  validate(_value: unknown, args: ValidationArguments): boolean {
    const volume = args.object as VolumeSpecDto
    return typeof volume.source === 'string' || typeof volume.host_path === 'string'
  }

  defaultMessage(): string {
    return 'volume requires source (or the deprecated host_path)'
  }
}

export class NetworkSpecDto {
  @IsIn(['enabled', 'disabled'])
  mode: 'enabled' | 'disabled'

  @IsOptional()
  @IsArray()
  @ArrayMaxSize(MAX_NETWORK_ALLOW_LIST_ENTRIES)
  @IsString({ each: true })
  @Validate(IsNetworkAllowEntryConstraint, { each: true })
  allow_net?: string[]
}

export class VolumeSpecDto {
  @IsOptional()
  @IsString()
  source?: string

  /**
   * @deprecated Use `source` with the `volume://<volume_id>` scheme instead.
   * Accepted for backward compatibility with existing /v1 clients built
   * against the pre-managed-volumes `VolumeSpec` schema; will be removed.
   */
  @IsOptional()
  @IsString()
  host_path?: string

  @IsString()
  @Validate(HasVolumeSourceConstraint)
  guest_path: string

  @ValidateIf((_, value) => value !== undefined)
  @IsIn([false])
  read_only?: false
}

export class CreateBoxDto {
  @IsOptional()
  @IsString()
  name?: string

  @IsOptional()
  @IsString()
  image?: string

  // A box with 0 vCPUs can never boot (libkrun set_vm_config(0, ...) → EINVAL),
  // so reject undersized resources at the request boundary instead of accepting
  // a box that fails to start.
  @IsOptional()
  @IsNumber()
  @Min(1)
  cpus?: number

  @IsOptional()
  @IsNumber()
  @Min(256)
  memory_mib?: number

  @IsOptional()
  @IsNumber()
  @Min(1)
  disk_size_gb?: number

  @IsOptional()
  @IsString()
  working_dir?: string

  @IsOptional()
  @IsObject()
  env?: Record<string, string>

  @IsOptional()
  @IsArray()
  entrypoint?: string[]

  @IsOptional()
  @IsArray()
  cmd?: string[]

  @IsOptional()
  @IsString()
  user?: string

  @IsOptional()
  @IsBoolean()
  detach?: boolean

  @IsOptional()
  @IsNumber()
  @Min(0)
  auto_stop?: number

  @IsOptional()
  @IsNumber()
  @Min(0)
  auto_delete?: number

  @IsOptional()
  @IsBoolean()
  auto_resume?: boolean

  @IsOptional()
  @ValidateNested()
  @Type(() => NetworkSpecDto)
  network?: NetworkSpecDto

  @IsOptional()
  @IsArray()
  @ValidateNested({ each: true })
  @Type(() => VolumeSpecDto)
  volumes?: VolumeSpecDto[]
}
