/*
 * Copyright 2025 BoxLite AI
 * SPDX-License-Identifier: AGPL-3.0
 */

import 'reflect-metadata'
import { validate } from 'class-validator'
import { plainToInstance } from 'class-transformer'
import { CreateVolumeDto } from './create-volume.dto'

// Same guard as the classic POST /volumes DTO's own spec
// (apps/api/src/box/dto/create-volume.dto.spec.ts): a name shaped like a
// UUID could collide with a different volume's real id in the same
// organization and make VolumeService.findOneByIdOrName's lookup ambiguous.
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

  it('rejects an empty name instead of silently falling back to the default', async () => {
    const errors = await validate(plainToInstance(CreateVolumeDto, { name: '' }))

    const fieldError = errors.find((e) => e.property === 'name')
    expect(fieldError?.constraints).toHaveProperty('isNotEmpty')
  })
})
