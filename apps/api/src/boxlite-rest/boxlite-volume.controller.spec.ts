import { BoxliteVolumeController } from './boxlite-volume.controller'
import { VolumeService } from '../box/services/volume.service'
import { NotFoundException } from '@nestjs/common'

describe('BoxliteVolumeController', () => {
  const createdAt = new Date('2026-07-27T00:00:00.000Z')
  const volume = { id: 'volume-1', createdAt }

  function createController() {
    const volumeService = {
      create: jest.fn().mockResolvedValue(volume),
      findAll: jest.fn().mockResolvedValue([volume]),
      findOne: jest.fn().mockResolvedValue(volume),
      delete: jest.fn().mockResolvedValue(undefined),
    }
    return {
      controller: new BoxliteVolumeController(volumeService as unknown as VolumeService),
      volumeService,
    }
  }

  it('creates an unbounded S3 volume', async () => {
    const { controller, volumeService } = createController()
    const organization = { id: 'org-1' }

    await expect(
      controller.create({
        organization,
        organizationId: organization.id,
      } as never),
    ).resolves.toEqual({
      id: volume.id,
      created_at: createdAt.toISOString(),
      size_bytes: 0,
    })
    expect(volumeService.create).toHaveBeenCalledWith(organization, {})
  })

  it('lists volumes for the authenticated organization', async () => {
    const { controller, volumeService } = createController()

    await expect(controller.list({ organizationId: 'org-1' } as never)).resolves.toEqual({
      volumes: [
        {
          id: volume.id,
          created_at: createdAt.toISOString(),
          size_bytes: 0,
        },
      ],
    })
    expect(volumeService.findAll).toHaveBeenCalledWith('org-1')
  })

  it('gets a volume by ID', async () => {
    const { controller, volumeService } = createController()

    await expect(controller.get(volume.id)).resolves.toEqual({
      id: volume.id,
      created_at: createdAt.toISOString(),
      size_bytes: 0,
    })
    expect(volumeService.findOne).toHaveBeenCalledWith(volume.id)
  })

  it('deletes a volume by ID', async () => {
    const { controller, volumeService } = createController()

    await expect(controller.remove(volume.id)).resolves.toBeUndefined()
    expect(volumeService.delete).toHaveBeenCalledWith(volume.id, false)
  })

  it('propagates a missing-volume error without force', async () => {
    const { controller, volumeService } = createController()
    const error = new NotFoundException('Volume not found')
    volumeService.delete.mockRejectedValueOnce(error)

    await expect(controller.remove(volume.id)).rejects.toBe(error)
  })

  it('passes force deletion semantics to the volume service', async () => {
    const { controller, volumeService } = createController()

    await expect(controller.remove(volume.id, 'true')).resolves.toBeUndefined()
    expect(volumeService.delete).toHaveBeenCalledWith(volume.id, true)
  })

  it('propagates non-not-found errors when force is true', async () => {
    const { controller, volumeService } = createController()
    const error = new Error('S3 unavailable')
    volumeService.delete.mockRejectedValueOnce(error)

    await expect(controller.remove(volume.id, 'true')).rejects.toBe(error)
  })
})
