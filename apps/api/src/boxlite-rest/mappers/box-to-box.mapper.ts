/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BadRequestException, Logger } from '@nestjs/common'
import { BoxDto } from '../../box/dto/box.dto'
import { BoxState } from '../../box/enums/box-state.enum'
import {
  AUTO_DELETE_DISABLED,
  DEFAULT_AUTO_STOP_SECONDS,
  DEFAULT_AUTO_RESUME,
} from '../../box/constants/box-lifecycle.constants'
import { BoxResponseDto } from '../dto/box-response.dto'
import { CreateBoxDto as RestCreateBoxDto } from '../dto/create-box.dto'
import { CreateBoxDto } from '../../box/dto/create-box.dto'

const logger = new Logger('BoxToBoxMapper')
const VOLUME_SCHEME = 'volume://'

export function boxToBoxResponse(box: BoxDto): BoxResponseDto {
  return {
    box_id: box.id,
    name: box.name,
    status: mapState(box.state),
    created_at: box.createdAt || new Date().toISOString(),
    updated_at: box.updatedAt || new Date().toISOString(),
    image: box.image || '',
    cpus: box.cpu || 1,
    memory_mib: (box.memory || 1) * 1024,
    labels: box.labels || {},
    auto_stop: box.autoStop ?? DEFAULT_AUTO_STOP_SECONDS,
    auto_delete: box.autoDelete ?? AUTO_DELETE_DISABLED,
    auto_resume: box.autoResume ?? DEFAULT_AUTO_RESUME,
  }
}

export function createBoxToCreateBox(dto: RestCreateBoxDto, target?: string): CreateBoxDto {
  const createDto = new CreateBoxDto()
  createDto.name = dto.name
  createDto.image = dto.image
  createDto.user = dto.user
  createDto.env = dto.env
  createDto.cpu = dto.cpus
  createDto.memory = dto.memory_mib ? Math.ceil(dto.memory_mib / 1024) : undefined
  createDto.disk = dto.disk_size_gb
  createDto.target = target
  createDto.autoStop = dto.auto_stop
  createDto.autoDelete = dto.auto_delete
  createDto.autoResume = dto.auto_resume
  createDto.volumes = dto.volumes?.map((spec) => ({
    volumeId: resolveVolumeId(spec),
    mountPath: spec.guest_path,
  }))
  if (dto.network) {
    const allowNet = dto.network.outbound?.allow_net?.map((entry) => entry.trim()).filter(Boolean)
    createDto.networkBlockAll = dto.network.outbound?.mode === 'disabled'
    createDto.networkAllowList =
      dto.network.outbound?.mode === 'enabled' && allowNet?.length ? allowNet.join(',') : undefined
    // The runner DTO only has a public/private boolean; a non-empty
    // inbound.allow_net never reaches here — the DTO rejects it at the
    // request boundary until enforcement exists.
    createDto.public = dto.network.inbound?.mode ? dto.network.inbound.mode === 'enabled' : undefined
  }
  return createDto
}

function resolveVolumeId(spec: NonNullable<RestCreateBoxDto['volumes']>[number]): string {
  if (spec.volume !== undefined && spec.host_path !== undefined) {
    throw new BadRequestException('volumes[] entries must not set both volume and the deprecated host_path')
  }
  // Bare id or name — VolumeService.validateVolumes (unaffected by this
  // mapper) resolves either against this organization's volumes.
  if (spec.volume !== undefined) {
    return spec.volume
  }
  // Deprecated pre-rename alias: DTO's HasVolumeReferenceConstraint already
  // guarantees one of `volume`/`host_path` is present, so getting here means
  // `host_path` is set. It must still carry the old `volume://<id>` scheme —
  // a bare path would be a genuine host-filesystem bind mount, which isn't
  // implemented (a REST box runs on a remote runner, so there's no path a
  // client could safely name there today).
  if (spec.host_path?.startsWith(VOLUME_SCHEME)) {
    const volumeId = spec.host_path.slice(VOLUME_SCHEME.length)
    if (volumeId) {
      logger.warn(
        'Deprecated: volumes[].host_path="volume://..." — use volumes[].volume instead. Support for host_path will be removed in a future release.',
      )
      return volumeId
    }
  }
  throw new BadRequestException(`host_path must use the ${VOLUME_SCHEME}<id> scheme (e.g. ${VOLUME_SCHEME}vol-123)`)
}

function mapState(state: string | BoxState | undefined): string {
  switch (state) {
    case BoxState.STARTED:
      return 'running'
    case BoxState.STOPPED:
    case BoxState.ARCHIVED:
      return 'stopped'
    case BoxState.CREATING:
    case BoxState.STARTING:
    case BoxState.RESTORING:
      return 'configured'
    case BoxState.STOPPING:
    case BoxState.DESTROYING:
    case BoxState.ARCHIVING:
      return 'stopping'
    case BoxState.ERROR:
    case BoxState.UNKNOWN:
    default:
      return 'unknown'
  }
}
