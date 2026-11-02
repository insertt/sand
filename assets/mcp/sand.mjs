// Shipped by sand. An MCP server on stdio that gives an agent sand runs over
// ACP the request_mount tool the `sand` mod gives Claude Code's own UI: it
// files the request in the bridge directory and returns at once; sand asks
// the person on its surface and tells the agent the outcome with the next
// message. No dependencies: the sandbox's Node is all it needs.
//
//   <requests dir>/<id>.json  { kind: 'mount', path, mode, reason }
import { randomUUID } from 'node:crypto'
import { writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { createInterface } from 'node:readline'

const REQUESTS = process.env.SAND_REQUESTS_DIR || `${homedir()}/.sand-requests`

const TOOL = {
  name: 'request_mount',
  description:
    'Ask the user to mount a directory of their own computer (the host) into this sandbox. Use it when the user asks ' +
    'for a host directory to be mounted, or when the work needs files outside the sandbox and the user agrees. ' +
    'The user confirms in sand. It returns at once; a later message says whether the directory was mounted. ' +
    'Once mounted it appears inside the sandbox at the same absolute path as on the host.',
  inputSchema: {
    type: 'object',
    properties: {
      path: {
        type: 'string',
        description:
          "The directory on the host as the user gave it: absolute, ~/... for the host user's home (not the " +
          "sandbox's), or relative to this sandbox's primary workspace. A trailing :ro or :rw, as in " +
          '`sand mount ~/notes:ro`, sets the mode; keep it as the user wrote it.',
      },
      mode: { type: 'string', enum: ['rw', 'ro'], description: 'Read-write (default) or read-only, when the path has no :ro or :rw.' },
      reason: { type: 'string', description: 'One short sentence on why, shown to the user.' },
    },
    required: ['path'],
  },
}

const text = (t, isError = false) => ({ content: [{ type: 'text', text: t }], isError })

/** Files the mount request; what the tool answers. */
function requestMount(args) {
  // `DIR:ro` / `DIR:rw`, as on sand's command line, wins over `mode`.
  const spec = typeof args?.path === 'string' ? args.path.trim() : ''
  const suffix = /:(ro|rw)$/.exec(spec)?.[1]
  const path = suffix ? spec.slice(0, -3) : spec
  if (path === '') return text('Give the host directory as `path`.', true)
  const mode = suffix ?? (args.mode === 'ro' ? 'ro' : 'rw')
  const reason = typeof args.reason === 'string' ? args.reason.slice(0, 300) : null
  try {
    writeFileSync(`${REQUESTS}/${randomUUID()}.json`, JSON.stringify({ kind: 'mount', path, mode, reason }))
  } catch (err) {
    return text(`sand could not ask the user: ${String(err)}`, true)
  }
  return text(
    `Asked the user to mount ${path} (${mode}). Do not wait for it: go on with other work or end your turn; ` +
      'a later message says whether it was mounted.',
  )
}

/** The answer to one JSON-RPC message; undefined for a notification. */
function handle(message) {
  const { id, method, params } = message
  if (id === undefined || id === null) return undefined
  const result = (r) => ({ jsonrpc: '2.0', id, result: r })
  switch (method) {
    case 'initialize':
      return result({
        protocolVersion: params?.protocolVersion ?? '2025-06-18',
        capabilities: { tools: {} },
        serverInfo: { name: 'sand', version: '1' },
      })
    case 'ping':
      return result({})
    case 'tools/list':
      return result({ tools: [TOOL] })
    case 'tools/call':
      if (params?.name !== TOOL.name) return { jsonrpc: '2.0', id, error: { code: -32602, message: `no tool ${params?.name}` } }
      return result(requestMount(params.arguments ?? {}))
    default:
      return { jsonrpc: '2.0', id, error: { code: -32601, message: `no method ${method}` } }
  }
}

createInterface({ input: process.stdin }).on('line', (line) => {
  let message
  try {
    message = JSON.parse(line)
  } catch {
    return
  }
  const answer = handle(message)
  if (answer) process.stdout.write(`${JSON.stringify(answer)}\n`)
})
