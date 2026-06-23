/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { BoxManager } from './box.manager'
import { BoxState } from '../enums/box-state.enum'

type Candidate = { id: string; runnerId: string | null }

function buildHarness(opts: {
  candidates: Candidate[]
  apiVersion?: string
  failedStartJobs?: number
  boxLocked?: boolean
  globalLockAcquired?: boolean
}) {
  const updateWhere = jest.fn().mockResolvedValue(undefined)

  const queryBuilder: any = {
    select: jest.fn().mockReturnThis(),
    where: jest.fn().mockReturnThis(),
    andWhere: jest.fn().mockReturnThis(),
    orderBy: jest.fn().mockReturnThis(),
    limit: jest.fn().mockReturnThis(),
    getMany: jest.fn().mockResolvedValue(opts.candidates),
  }
  const boxRepository: any = {
    createQueryBuilder: jest.fn().mockReturnValue(queryBuilder),
    updateWhere,
  }

  const runnerService: any = {
    getRunnerApiVersion: jest.fn().mockResolvedValue(opts.apiVersion ?? '2'),
  }

  const redisLockProvider: any = {
    lock: jest.fn().mockResolvedValue(opts.globalLockAcquired ?? true),
    unlock: jest.fn().mockResolvedValue(undefined),
    isLocked: jest.fn().mockResolvedValue(opts.boxLocked ?? false),
  }

  const jobRepository: any = {
    count: jest.fn().mockResolvedValue(opts.failedStartJobs ?? 0),
  }

  const manager = new BoxManager(
    boxRepository,
    runnerService,
    redisLockProvider,
    {} as any,
    {} as any,
    {} as any,
    jobRepository,
  )

  return { manager, updateWhere, runnerService, redisLockProvider, jobRepository }
}

describe('BoxManager.reconcileErroredBoxes', () => {
  afterEach(() => jest.clearAllMocks())

  it('flips a recoverable ERROR box to STOPPED so the start flow can re-drive it', async () => {
    const { manager, updateWhere } = buildHarness({
      candidates: [{ id: 'box-1', runnerId: 'runner-1' }],
    })

    await manager.reconcileErroredBoxes()

    expect(updateWhere).toHaveBeenCalledTimes(1)
    expect(updateWhere).toHaveBeenCalledWith('box-1', {
      updateData: { state: BoxState.STOPPED, errorReason: null },
      whereCondition: { state: BoxState.ERROR },
    })
  })

  it('does not retry a box that already hit the recovery attempt ceiling', async () => {
    const { manager, updateWhere, jobRepository } = buildHarness({
      candidates: [{ id: 'box-1', runnerId: 'runner-1' }],
      failedStartJobs: 5, // MAX_RECOVER_ATTEMPTS
    })

    await manager.reconcileErroredBoxes()

    expect(jobRepository.count).toHaveBeenCalledTimes(1)
    expect(updateWhere).not.toHaveBeenCalled()
  })

  it('skips boxes on non-v2 runners', async () => {
    const { manager, updateWhere, jobRepository } = buildHarness({
      candidates: [{ id: 'box-1', runnerId: 'runner-1' }],
      apiVersion: '1',
    })

    await manager.reconcileErroredBoxes()

    expect(jobRepository.count).not.toHaveBeenCalled()
    expect(updateWhere).not.toHaveBeenCalled()
  })

  it('skips boxes the sync loop is already holding a lock on', async () => {
    const { manager, updateWhere } = buildHarness({
      candidates: [{ id: 'box-1', runnerId: 'runner-1' }],
      boxLocked: true,
    })

    await manager.reconcileErroredBoxes()

    expect(updateWhere).not.toHaveBeenCalled()
  })

  it('bails out without scanning when the global lock is held by another worker', async () => {
    const { manager, updateWhere, redisLockProvider } = buildHarness({
      candidates: [{ id: 'box-1', runnerId: 'runner-1' }],
      globalLockAcquired: false,
    })

    await manager.reconcileErroredBoxes()

    expect(updateWhere).not.toHaveBeenCalled()
    expect(redisLockProvider.unlock).not.toHaveBeenCalled()
  })
})
