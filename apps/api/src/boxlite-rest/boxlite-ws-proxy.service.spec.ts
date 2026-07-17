/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { createProxyMiddleware } from 'http-proxy-middleware'
import { EventEmitter } from 'events'
import { request as httpRequest, type IncomingMessage } from 'http'
import type { Socket } from 'net'
import { BoxliteWsProxyService } from './boxlite-ws-proxy.service'

jest.mock('http', () => ({
  ...jest.requireActual('http'),
  request: jest.fn(),
}))
jest.mock('http-proxy-middleware', () => ({
  createProxyMiddleware: jest.fn(() => ({
    upgrade: jest.fn(),
  })),
}))
jest.mock('uuid', () => ({
  v4: jest.fn(() => 'mock-uuid'),
  validate: jest.fn(() => true),
}))

describe('BoxliteWsProxyService', () => {
  beforeEach(() => {
    jest.clearAllMocks()
  })

  function authRequest(token: string, url = '/api/v1/org-1/boxes/public-box/executions/exec-1/attach') {
    return {
      url,
      headers: {
        authorization: `Bearer ${token}`,
      },
    } as IncomingMessage
  }

  function buildAuthHarness() {
    const apiKeyService = {
      getApiKeyByValue: jest.fn().mockRejectedValue(new Error('api key not found')),
    }
    const organizationUserService = {
      findOne: jest.fn(),
    }
    const jwtStrategy = {
      verifyToken: jest.fn(),
    }
    const service = new BoxliteWsProxyService(
      apiKeyService as never,
      organizationUserService as never,
      {} as never,
      {} as never,
      jwtStrategy as never,
    ) as unknown as {
      authenticate: (req: IncomingMessage, urlTenant?: string) => Promise<{ organizationId: string } | null>
    }

    return { service, apiKeyService, organizationUserService, jwtStrategy }
  }

  it('rewrites public box ids to internal box ids before proxying attach upgrades to the runner', () => {
    new BoxliteWsProxyService({} as never, {} as never, {} as never, {} as never, {} as never)

    const proxyOptions = jest.mocked(createProxyMiddleware).mock.calls[0][0]
    const pathRewrite = proxyOptions.pathRewrite as (path: string, req: unknown) => string
    const req = { __boxliteRunnerBoxId: 'box-uuid' }

    expect(pathRewrite('/api/v1/boxes/public-box/executions/exec-1/attach', req)).toBe(
      '/v1/boxes/box-uuid/executions/exec-1/attach',
    )
    expect(pathRewrite('/api/v1/default/boxes/public-box/executions/exec-1/attach?x=1', req)).toBe(
      '/v1/boxes/box-uuid/executions/exec-1/attach?x=1',
    )
  })

  it('acknowledges a successful CONNECT before relaying bytes', async () => {
    const apiKeyService = {
      getApiKeyByValue: jest.fn().mockResolvedValue({
        organizationId: 'org-1',
        userId: 'user-1',
        expiresAt: null,
      }),
    }
    const organizationUserService = {
      findOne: jest.fn().mockResolvedValue({ organizationId: 'org-1', userId: 'user-1' }),
    }
    const boxService = {
      findOneByIdOrName: jest.fn().mockResolvedValue({ id: 'internal-box', runnerId: 'runner-1' }),
    }
    const runnerService = {
      findOne: jest.fn().mockResolvedValue({
        apiUrl: 'http://runner.internal:3003',
        apiKey: 'runner-key',
      }),
    }
    const service = new BoxliteWsProxyService(
      apiKeyService as never,
      organizationUserService as never,
      boxService as never,
      runnerService as never,
      {} as never,
    )

    const upstreamRequest = new EventEmitter() as EventEmitter & { end: jest.Mock }
    upstreamRequest.end = jest.fn()
    jest.mocked(httpRequest).mockReturnValue(upstreamRequest as never)

    const clientSocket = new EventEmitter() as EventEmitter & {
      write: jest.Mock
      pipe: jest.Mock
      destroy: jest.Mock
    }
    clientSocket.write = jest.fn()
    clientSocket.pipe = jest.fn()
    clientSocket.destroy = jest.fn()
    const upstreamSocket = new EventEmitter() as EventEmitter & {
      pipe: jest.Mock
      destroy: jest.Mock
    }
    upstreamSocket.pipe = jest.fn()
    upstreamSocket.destroy = jest.fn()

    await service.connect(
      authRequest('blk_live_test', '/api/v1/org-1/boxes/public-box/network/tunnel?port=3000'),
      clientSocket as unknown as Socket,
    )
    upstreamRequest.emit('connect', { statusCode: 200 }, upstreamSocket, Buffer.from('upstream-head'))

    expect(httpRequest).toHaveBeenCalledWith(
      expect.objectContaining({
        method: 'CONNECT',
        path: '/v1/boxes/internal-box/network/tunnel?port=3000',
        headers: { Authorization: 'Bearer runner-key' },
      }),
    )
    expect(clientSocket.write).toHaveBeenNthCalledWith(1, 'HTTP/1.1 200 Connection Established\r\n\r\n')
    expect(clientSocket.write).toHaveBeenNthCalledWith(2, Buffer.from('upstream-head'))
    expect(clientSocket.pipe).toHaveBeenCalledWith(upstreamSocket)
    expect(upstreamSocket.pipe).toHaveBeenCalledWith(clientSocket)
  })

  it('authenticates API key bearer tokens for websocket attach', async () => {
    const { service, apiKeyService, organizationUserService, jwtStrategy } = buildAuthHarness()
    apiKeyService.getApiKeyByValue.mockResolvedValue({
      organizationId: 'org-1',
      userId: 'user-1',
      expiresAt: null,
    })
    organizationUserService.findOne.mockResolvedValue({ organizationId: 'org-1', userId: 'user-1' })

    await expect(service.authenticate(authRequest('blk_live_test'))).resolves.toEqual({ organizationId: 'org-1' })
    expect(organizationUserService.findOne).toHaveBeenCalledWith('org-1', 'user-1')
    expect(jwtStrategy.verifyToken).not.toHaveBeenCalled()
  })

  it('authenticates JWT bearer tokens for websocket attach', async () => {
    const { service, organizationUserService, jwtStrategy } = buildAuthHarness()
    const jwt = 'eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyXzEifQ.signature'
    jwtStrategy.verifyToken.mockResolvedValue({ sub: 'user-1', email: 'dev@acme.test' })
    organizationUserService.findOne.mockResolvedValue({ organizationId: 'org-1', userId: 'user-1' })

    await expect(service.authenticate(authRequest(jwt), 'org-1')).resolves.toEqual({ organizationId: 'org-1' })
    expect(jwtStrategy.verifyToken).toHaveBeenCalledWith(jwt)
    expect(organizationUserService.findOne).toHaveBeenCalledWith('org-1', 'user-1')
  })

  it('rejects invalid JWT bearer tokens for websocket attach', async () => {
    const { service, organizationUserService, jwtStrategy } = buildAuthHarness()
    const jwt = 'eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyXzEifQ.signature'
    jwtStrategy.verifyToken.mockRejectedValue(new Error('bad jwt'))

    await expect(service.authenticate(authRequest(jwt), 'org-1')).resolves.toBeNull()
    expect(organizationUserService.findOne).not.toHaveBeenCalled()
  })

  it('rejects JWT attach when organization membership has been removed', async () => {
    const { service, organizationUserService, jwtStrategy } = buildAuthHarness()
    const jwt = 'eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyXzEifQ.signature'
    jwtStrategy.verifyToken.mockResolvedValue({ sub: 'user-1', email: 'dev@acme.test' })
    organizationUserService.findOne.mockResolvedValue(null)

    await expect(service.authenticate(authRequest(jwt), 'org-1')).resolves.toBeNull()
    expect(organizationUserService.findOne).toHaveBeenCalledWith('org-1', 'user-1')
  })
})
