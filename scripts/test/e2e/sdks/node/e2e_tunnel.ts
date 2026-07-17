// REST E2E driver for the Node SDK's public box.network.tunnel(port) API.

import { setTimeout as delay } from 'node:timers/promises'
import { ApiKeyCredential, BoxliteRestOptions, JsBoxlite, SimpleBox } from '../../../../../sdks/node'

const GUEST_PORT = 18081
const MARKER = 'node-sdk-tunnel-e2e'

function env(name: string, fallback: string): string {
  const value = process.env[name]
  return value && value.length ? value : fallback
}

async function getOverTunnel(box: SimpleBox): Promise<string> {
  const tunnel = await box.network.tunnel(GUEST_PORT)
  const endpoint = await tunnel.endpoint()
  if (typeof endpoint !== 'string') {
    throw new Error('expected REST tunnel endpoint URL for the cloud box')
  }
  const socket = await tunnel.connect()
  return await new Promise<string>((resolve, reject) => {
    let response = ''
    let settled = false
    let timer: ReturnType<typeof setTimeout>

    const finish = (error?: Error) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      socket.destroy()
      if (error) reject(error)
      else resolve(response)
    }

    timer = setTimeout(() => finish(new Error('HTTP response timed out')), 5_000)
    socket.setEncoding('utf8')
    socket.once('error', (error) => finish(error))
    socket.on('data', (chunk: string) => {
      response += chunk
      if (response.includes(MARKER)) finish()
    })
    socket.once('end', () => finish())
    socket.write('GET /tunnel-e2e-node.txt HTTP/1.0\r\nHost: tunnel.test\r\n\r\n')
  })
}

async function waitForHttp(box: SimpleBox): Promise<string> {
  const deadline = Date.now() + 30_000
  let lastError: unknown
  while (Date.now() < deadline) {
    try {
      const response = await getOverTunnel(box)
      if (response.includes(MARKER)) return response
      lastError = new Error(`unexpected HTTP response: ${response}`)
    } catch (error) {
      lastError = error
    }
    await delay(250)
  }
  throw new Error(`guest HTTP service was not reachable through tunnel: ${String(lastError)}`)
}

async function main(): Promise<void> {
  const runtime = JsBoxlite.rest(
    new BoxliteRestOptions({
      url: env('BOXLITE_E2E_URL', 'http://localhost:3000/api'),
      credential: new ApiKeyCredential(env('BOXLITE_E2E_API_KEY', 'devkey')),
      pathPrefix: env('BOXLITE_E2E_PREFIX', ''),
    }),
  )
  const box = new SimpleBox({
    image: env('BOXLITE_E2E_IMAGE', 'ghcr.io/boxlite-ai/boxlite-agent-base:20260605-p0-r3'),
    autoRemove: true,
    runtime,
  })

  try {
    const server = await box.exec('sh', [
      '-lc',
      `printf '%s\\n' '${MARKER}' > /root/tunnel-e2e-node.txt; ` +
        `python3 -m http.server ${GUEST_PORT} --bind 0.0.0.0 --directory /root ` +
        '>/tmp/tunnel-e2e-node.log 2>&1 &',
    ])
    if (server.exitCode !== 0) {
      throw new Error(`failed to start HTTP server: ${server.stderr}`)
    }

    const response = await waitForHttp(box)
    if (!response.startsWith('HTTP/1.0 200') && !response.startsWith('HTTP/1.1 200')) {
      throw new Error(`unexpected HTTP status: ${response.slice(0, 80)}`)
    }
    console.log('TUNNEL_HTTP=ok')
  } finally {
    await box.stop().catch(() => undefined)
    runtime.close()
  }
}

void main().catch((error: unknown) => {
  console.error(error)
  process.exitCode = 1
})
