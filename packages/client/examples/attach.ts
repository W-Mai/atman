import { AtmanClient, FetchTransport } from '@atman/client'

const token = process.env.ATMAN_DAEMON_TOKEN
if (!token) throw new Error('ATMAN_DAEMON_TOKEN is required')

const client = await AtmanClient.connect(
  new FetchTransport({ baseUrl: 'http://127.0.0.1:65099', token }),
  { name: 'typescript-example', version: 'example' },
)
const snapshot = await client.command('session.create', {
  request_id: crypto.randomUUID(),
  project_root: process.cwd(),
  title: 'SDK example',
})
const session = await client.attachSession(snapshot.projection.metadata.id)

const unsubscribe = session.subscribe((current) => {
  console.log(
    `cursor=${current.cursor} messages=${current.projection.transcript?.length ?? 0}`,
  )
})
const stop = new AbortController()
const synchronization = session.synchronize({ signal: stop.signal }).catch((error) => {
  if (!stop.signal.aborted) throw error
})

await session.sendMessage('Summarize the current workspace.')
await new Promise((resolve) => setTimeout(resolve, 1_000))
stop.abort()
await synchronization
unsubscribe()
