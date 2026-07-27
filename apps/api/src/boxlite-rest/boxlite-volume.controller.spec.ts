import { BoxliteVolumeController } from './boxlite-volume.controller'
import { VolumeService } from '../box/services/volume.service'

describe('BoxliteVolumeController', () => {
  const createdAt = new Date('2026-07-27T00:00:00.000Z')
  const volume = { id: 'volume-1', createdAt }

  function createController() {
    const volumeService = {
      create: jest.fn().mockResolvedValue(volume),
      findAll: jest.fn().mockResolvedValue([volume]),
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
})
