/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BadRequestException, Logger } from '@nestjs/common'
import { Transform, Type, plainToInstance } from 'class-transformer'
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
  ValidateNested,
  ValidatorConstraint,
  ValidatorConstraintInterface,
} from 'class-validator'
import { isValidNetworkAllowEntry, MAX_NETWORK_ALLOW_LIST_ENTRIES } from '../../box/utils/network-validation.util'

const logger = new Logger('CreateBoxDto')

@ValidatorConstraint({ name: 'isNetworkAllowEntry', async: false })
class IsNetworkAllowEntryConstraint implements ValidatorConstraintInterface {
  validate(value: unknown): boolean {
    return typeof value === 'string' && isValidNetworkAllowEntry(value)
  }

  defaultMessage(): string {
    return 'each allow_net entry must be an IPv4 address, IPv4 CIDR, hostname, or wildcard hostname'
  }
}

export class OutboundNetworkSpecDto {
  @IsIn(['enabled', 'disabled'])
  mode: 'enabled' | 'disabled'

  @IsOptional()
  @IsArray()
  @ArrayMaxSize(MAX_NETWORK_ALLOW_LIST_ENTRIES)
  @IsString({ each: true })
  @Validate(IsNetworkAllowEntryConstraint, { each: true })
  allow_net?: string[]
}

// Rejects any non-empty inbound allowlist: no layer enforces it yet (the
// proxy gates purely on mode), so accepting one would hand the caller a
// box that is fully open while they believe it is restricted. Lift this
// once inbound allowlist enforcement lands.
@ValidatorConstraint({ name: 'isUnsupportedInboundAllowNet', async: false })
class IsUnsupportedInboundAllowNetConstraint implements ValidatorConstraintInterface {
  validate(value: unknown): boolean {
    return value === undefined || (Array.isArray(value) && value.length === 0)
  }

  defaultMessage(): string {
    return 'inbound.allow_net is not supported yet; remove it (inbound access is controlled by mode only)'
  }
}

// Aligned field-for-field with OutboundNetworkSpecDto: mode="enabled" means
// services the box exposes are publicly reachable; mode="disabled" means
// private. allow_net exists for wire-shape symmetry but is rejected when
// non-empty until enforcement exists.
export class InboundNetworkSpecDto {
  @IsIn(['enabled', 'disabled'])
  mode: 'enabled' | 'disabled'

  @IsOptional()
  @Validate(IsUnsupportedInboundAllowNetConstraint)
  allow_net?: string[]
}

export class NetworkSpecDto {
  @IsOptional()
  @IsObject()
  @ValidateNested()
  @Type(() => OutboundNetworkSpecDto)
  outbound?: OutboundNetworkSpecDto

  @IsOptional()
  @IsObject()
  @ValidateNested()
  @Type(() => InboundNetworkSpecDto)
  inbound?: InboundNetworkSpecDto
}

// Deprecated legacy wire shape, predating the outbound/inbound split:
// `{ mode, allow_net, service_access }` at the top level of `network`,
// instead of nested under `outbound`/`inbound`. Still accepted so already-
// deployed callers keep working; normalized into the nested shape here and
// logged so callers can be tracked down and migrated. `service_access`
// mapped `'public'`/`'private'` to what is now `inbound.mode`. Mixing legacy
// and nested fields in the same request is rejected outright — there's no
// sane precedence to guess between them.
function normalizeNetworkShape(value: unknown): NetworkSpecDto | unknown {
  if (value === undefined || value === null || typeof value !== 'object' || Array.isArray(value)) {
    return value
  }
  const network = value as Record<string, unknown>
  const hasLegacyField = 'mode' in network || 'allow_net' in network || 'service_access' in network
  const hasNestedField = 'outbound' in network || 'inbound' in network

  if (hasLegacyField && hasNestedField) {
    throw new BadRequestException('network must not mix legacy top-level fields with nested outbound/inbound fields')
  }
  if (!hasLegacyField) {
    return plainToInstance(NetworkSpecDto, network)
  }

  logger.warn(
    'Deprecated: network.{mode,allow_net,service_access} — use network.{outbound,inbound}. Support for the flat shape will be removed in a future release.',
  )

  const { mode, allow_net, service_access, ...rest } = network
  if (service_access !== undefined && service_access !== 'public' && service_access !== 'private') {
    throw new BadRequestException(
      `network.service_access must be "public" or "private", got ${JSON.stringify(service_access)}`,
    )
  }
  // allow_net alone (no explicit mode) implies enabled, matching outbound's
  // existing default — an allowlist with nothing to enable would be inert.
  const outbound = mode !== undefined || allow_net !== undefined ? { mode: mode ?? 'enabled', allow_net } : undefined
  const inbound =
    service_access !== undefined ? { mode: service_access === 'public' ? 'enabled' : 'disabled' } : undefined
  return plainToInstance(NetworkSpecDto, { ...rest, outbound, inbound })
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
  @IsObject()
  @Transform(({ value }) => normalizeNetworkShape(value))
  @ValidateNested()
  network?: NetworkSpecDto
}
