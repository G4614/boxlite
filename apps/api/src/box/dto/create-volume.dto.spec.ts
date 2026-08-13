/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import 'reflect-metadata'
import { validate } from 'class-validator'
import { plainToInstance } from 'class-transformer'
import { CreateVolumeDto } from './create-volume.dto'

// VolumeService.findOneByIdOrName/validateVolumes match ids and names
// through the same lookup, so a name shaped like a UUID could collide with
// another volume's real id in the same organization and make that lookup
// ambiguous. Rejecting the UUID shape at the request boundary keeps the two
// namespaces disjoint by construction.
describe('CreateVolumeDto name', () => {
  it('rejects a UUID-shaped name', async () => {
    const errors = await validate(plainToInstance(CreateVolumeDto, { name: '550e8400-e29b-41d4-a716-446655440000' }))

    const fieldError = errors.find((e) => e.property === 'name')
    expect(fieldError?.constraints).toHaveProperty('isNotUuidShaped')
  })

  it('accepts an ordinary name', async () => {
    const errors = await validate(plainToInstance(CreateVolumeDto, { name: 'my-vol' }))

    expect(errors).toHaveLength(0)
  })

  it('accepts a request that omits name (defaults to the server-assigned id)', async () => {
    const errors = await validate(plainToInstance(CreateVolumeDto, {}))

    expect(errors).toHaveLength(0)
  })
})
