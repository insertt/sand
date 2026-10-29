// Shipped by sand. When the sandbox network policy blocks a host, or the agent
// asks to mount a host directory (its request_mount tool), a row above the
// prompt offers to allow it. The choice is only a request: sand, on the host,
// confirms it in a native dialog the agent cannot answer, applies it and writes
// back the result. The bridge directory is mounted from the host, one per
// sandbox:
//
//   pending.json        sand: the sbx approvals waiting for this sandbox
//   requests/<id>.json  this mod: { host, approval, option } or { kind: 'mount', path, mode, reason }
//   results/<id>.json   sand: { ok, allowed, message }
import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register, ToolCallResult } from 'claude-code'

import type { Block, Choice } from '../types'

const blocks = atom({ plugin: 'sand', key: 'blocks' } as const, [], { shape: 'blocks-1' })

const HOST = '([A-Za-z0-9][A-Za-z0-9.-]*(?::\\d+)?)'
const NEEDS_APPROVAL = new RegExp(`Approval required for ${HOST}`, 'g')
const DEFAULT_DENY = new RegExp(`Blocked by network policy: domain ${HOST}`, 'g')
const LOCAL_RULE = new RegExp(`Blocked by local rule for ${HOST}`, 'g')
const ORG_POLICY = /Blocked by org policy/

/** Without an sbx approval, allowing adds a rule for this sandbox alone. */
const SCOPED: Choice[] = [
  { id: 'allow', label: 'Allow for this sandbox', isDeny: false },
  { id: 'dismiss', label: 'Dismiss', isDeny: true },
]
/** A deny rule (an earlier refusal) blocks the host: sand lifts it for this sandbox on allowing. */
const RULED: Choice[] = [
  { id: 'allow', label: 'Lift the deny rule and allow', isDeny: false },
  { id: 'dismiss', label: 'Dismiss', isDeny: true },
]
const SHOWN = 3
const MOUNT_TOOL = 'mcp__sand__request_mount'
const MOUNT_DESCRIPTION =
  "Ask the user to mount a directory of their own computer (the host) into this sandbox. Use it when the user asks " +
  'for a host directory to be mounted, or when the work needs files outside the sandbox and the user agrees. ' +
  'The user confirms in Claude Code and then on the host. It returns at once; a later message says whether the ' +
  'directory was mounted. Once mounted it appears inside the sandbox at the same absolute path as on the host.'
const MOUNT_SCHEMA = {
  type: 'object',
  properties: {
    path: {
      type: 'string',
      description:
        "The directory on the host as the user gave it: absolute, ~/... for the host user's home (not the sandbox's), " +
        "or relative to this sandbox's primary workspace. A trailing :ro or :rw, as in `sand mount ~/notes:ro`, " +
        'sets the mode; keep it as the user wrote it.',
    },
    mode: { type: 'string', enum: ['rw', 'ro'], description: 'Read-write (default) or read-only, when the path has no :ro or :rw.' },
    reason: { type: 'string', description: 'One short sentence on why, shown to the user.' },
  },
  required: ['path'],
}
const mountChoices = (mode: 'ro' | 'rw'): Choice[] => {
  const rw = { id: 'rw', label: 'Mount read-write', isDeny: false }
  const ro = { id: 'ro', label: 'Mount read-only', isDeny: false }
  return [...(mode === 'ro' ? [ro, rw] : [rw, ro]), { id: 'dismiss', label: 'Dismiss', isDeny: true }]
}
/** Approvals already answered, kept out until sand's next pending list drops them. */
const answered = new Set<string>()

type Pending = { id: string; host: string; options: { id: string; label: string }[] }
type Result = { ok: boolean; allowed: boolean; message: string }

// A sentence ending right after the host ("...for example.com.") leaves its period.
const cleanHost = (host: string) => host.replace(/\.+$/, '').toLowerCase()
const isDenyOption = (o: { id: string; label: string }) => /deny|reject|block|decline/i.test(`${o.id} ${o.label}`)

const hostsIn = (text: string, pattern: RegExp) => [...text.matchAll(pattern)].map(m => cleanHost(m[1] ?? ''))

const bridgeDir = async ($: EngineInterface) =>
  (await $.env.get('SAND_POLICY_DIR')) ?? `${(await $.env.get('HOME')) ?? '/home/agent'}/.sand-policy`

const readJson = async <T,>($: EngineInterface, path: string): Promise<T | undefined> => {
  const text = await $.fs.read(path).catch(() => undefined)
  if (text === undefined) return undefined
  try {
    return JSON.parse(text) as T
  } catch {
    return undefined
  }
}

/**
 * A desktop notification, so a blocked agent is noticed away from the terminal.
 * Claude Code raises it with its terminal's escape sequence, which reaches the
 * host terminal through sbx: the notification is the terminal's own.
 */
const notify = async ($: EngineInterface, hosts: string[]) => {
  const sandbox = (await $.env.get('SANDBOX_NAME')) ?? 'The sandbox'
  await $.ui.notify(`${sandbox} is blocked from reaching ${hosts.join(', ')}. Answer in Claude Code.`).catch(() => undefined)
}

/** Adds a row for each newly blocked host, `ruled` the ones a deny rule blocks; answers the hosts it added. */
const report = async ($: EngineInterface, hosts: string[], ruled: string[]) => {
  const known = new Set((await read($, blocks)).map(b => b.host))
  const added = [...new Set([...hosts, ...ruled])].filter(host => !known.has(host))
  if (added.length === 0) return added
  await update($, blocks, list => [
    ...list,
    ...added.map((host): Block => {
      const isRuled = ruled.includes(host)
      return { id: crypto.randomUUID(), host, choices: isRuled ? RULED : SCOPED, status: 'open', isRuled }
    }),
  ])
  $.ui.toast(`Sandbox blocked ${added.join(', ')}: allow or dismiss it above the prompt`, { timeoutMs: 8000 })
  await notify($, added)
  return added
}

const choose = async ($: EngineInterface, block: Block, choice: Choice) => {
  if (choice.isDeny && block.approval === undefined) {
    await update($, blocks, list => list.filter(b => b.id !== block.id))
    return
  }
  const request = block.mount
    ? { kind: 'mount', path: block.host, mode: choice.id, reason: block.mount.reason ?? null }
    : { host: block.host, approval: block.approval ?? null, option: block.approval ? choice.id : null }
  try {
    await $.fs.write(`${await bridgeDir($)}/requests/${block.id}.json`, JSON.stringify(request))
  } catch (err) {
    $.ui.toast(`Could not ask sand to change the policy: ${String(err)}`)
    return
  }
  await update($, blocks, list => list.map(b => (b.id === block.id ? { ...b, status: 'sent' } : b)))
}

/** Picks up the approvals sand lists and the results it wrote back. */
const poll = async ($: EngineInterface) => {
  const dir = await bridgeDir($)
  const pending = await readJson<{ approvals?: Pending[] }>($, `${dir}/pending.json`)
  if (pending?.approvals) {
    const approvals = pending.approvals.filter(
      a => typeof a.id === 'string' && typeof a.host === 'string' && !answered.has(a.id),
    )
    const fresh: string[] = []
    await update($, blocks, list => {
      const ids = new Set(approvals.map(a => a.id))
      // An open approval that is no longer pending was answered elsewhere (sbx on the host).
      let next = list.filter(b => b.status === 'sent' || b.approval === undefined || ids.has(b.approval))
      for (const a of approvals) {
        const host = cleanHost(a.host)
        const choices = (a.options ?? []).map(o => ({ id: o.id, label: o.label || o.id, isDeny: isDenyOption(o) }))
        const patch = { approval: a.id, choices: choices.length > 0 ? choices : SCOPED }
        const same = next.find(b => b.approval === a.id || (b.approval === undefined && b.host === host))
        if (same === undefined) {
          fresh.push(host)
          next = [...next, { id: crypto.randomUUID(), host, status: 'open', ...patch }]
        } else if (same.status === 'open' && same.approval === undefined) {
          next = next.map(b => (b === same ? { ...b, ...patch } : b))
        }
      }
      return next
    })
    if (fresh.length > 0) {
      $.ui.toast(`Sandbox needs approval for ${fresh.join(', ')}: answer it above the prompt`, { timeoutMs: 8000 })
      await notify($, fresh)
    }
  }

  for (const block of (await read($, blocks)).filter(b => b.status === 'sent')) {
    const result = await readJson<Result>($, `${dir}/results/${block.id}.json`)
    if (result === undefined) continue
    if (block.approval !== undefined) answered.add(block.approval)
    await update($, blocks, list => list.filter(b => b.id !== block.id))
    $.ui.toast(result.message || (result.ok ? `${block.host}: done` : `${block.host}: failed`), { timeoutMs: 6000 })
    if (result.ok && result.allowed) {
      const text = block.mount
        ? `[sand] The user agreed and sand ${result.message}; it is available in the sandbox now.`
        : `[sand] The user allowed ${block.host} for this sandbox. ` +
          'Requests to it that were blocked earlier failed; retry them if they are still needed.'
      await $.session.append({ message: { type: 'user', content: [{ type: 'text', text }] } })
    } else if (block.mount) {
      const text = `[sand] The directory ${block.host} was not mounted: ${result.message}.`
      await $.session.append({ message: { type: 'user', content: [{ type: 'text', text }] } })
    }
  }
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    // Without the tool the network prompts still work.
    await $.tool
      .register({ name: 'request_mount', description: MOUNT_DESCRIPTION, inputSchema: MOUNT_SCHEMA, isDeferred: false })
      .catch(() => undefined)
    let isBusy = false
    $.clock.every(1500, () => {
      if (isBusy) return
      isBusy = true
      void poll($).finally(() => {
        isBusy = false
      })
    })
    return next(e)
  })

  on('tool.call', { tool: MOUNT_TOOL }, async ($, e) => {
    // A plugin tool's result is its text; an error is a deny, which the model reads as the tool's error.
    const answer = (text: string) => ({ result: text })
    // `DIR:ro` / `DIR:rw`, as on sand's command line, wins over `mode`.
    const spec = typeof e.path === 'string' ? e.path.trim() : ''
    const suffix = /:(ro|rw)$/.exec(spec)?.[1] as 'ro' | 'rw' | undefined
    const path = suffix ? spec.slice(0, -3) : spec
    if (path === '') return { deny: 'Give the host directory as `path`.' }
    const mode = suffix ?? (e.mode === 'ro' ? 'ro' : 'rw')
    const reason = typeof e.reason === 'string' ? e.reason.slice(0, 300) : undefined
    const known = (await read($, blocks)).some(b => b.mount && b.host === path && b.status === 'open')
    if (!known) {
      const block: Block = { id: crypto.randomUUID(), host: path, choices: mountChoices(mode), status: 'open', mount: { reason } }
      await update($, blocks, list => [...list, block])
      $.ui.toast(`The agent asks to mount ${path}: answer it above the prompt`, { timeoutMs: 8000 })
      const sandbox = (await $.env.get('SANDBOX_NAME')) ?? 'The sandbox'
      await $.ui.notify(`${sandbox} asks to mount ${path}. Answer in Claude Code.`).catch(() => undefined)
    }
    return answer(
      `Asked the user to mount ${path} (${mode}). Do not wait for it: go on with other work or end your turn; ` +
        'a later message says whether it was mounted.',
    )
  }).catch(() => ({ deny: 'sand could not ask the user.' }))

  on('tool.call', async ($, e, next) => {
    const ran = await next(e)
    if (ran.deny !== undefined || !ran.text) return ran
    const text = ran.text

    if (ORG_POLICY.test(text)) {
      $.ui.toast("A request was blocked by your organization's policy; ask your IT team if you need access")
    }
    const blocked = [...hostsIn(text, NEEDS_APPROVAL), ...hostsIn(text, DEFAULT_DENY)]
    const added = await report($, blocked, hostsIn(text, LOCAL_RULE))
    if (added.length === 0) return ran

    const note =
      `[sand] The user is being asked whether to allow ${added.join(', ')}. ` +
      'Do not retry or work around the block until told it is allowed; go on with other work or tell the user what you need.'
    return { ...ran, context: [...(ran.context ?? []), note] } as ToolCallResult
  }).catch(($, e, next) => next(e))

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const list = await read($, blocks)
    if (e.props.hasSurvey || list.length === 0) return next(e)

    const { Box, Button, Text } = $.ui.resolve(e)
    return (
      <Box flexDirection="column">
        {list.slice(0, SHOWN).map(block => (
          <Box key={block.id} flexWrap="wrap" columnGap={1}>
            <Text color={block.mount ? 'yellow' : 'red'}>{block.mount ? '📁' : '⛔'}</Text>
            <Text bold>{block.host}</Text>
            {block.status === 'sent' ? (
              <Text dimColor>waiting for you to confirm on the host…</Text>
            ) : (
              <Text dimColor>
                {block.mount
                  ? `the agent asks to mount this host directory${block.mount.reason ? `: ${block.mount.reason}` : ''}`
                  : block.approval
                  ? 'needs approval'
                  : block.isRuled
                    ? 'blocked by a deny rule (refused before)'
                    : 'blocked by the sandbox network policy'}
              </Text>
            )}
            {block.status === 'open' &&
              block.choices.map(choice => (
                <Button
                  key={`${block.id}:${choice.id}`}
                  label={choice.label}
                  variant={choice.isDeny ? 'secondary' : 'primary'}
                  onPress={() => void choose($, block, choice)}
                />
              ))}
          </Box>
        ))}
        {list.length > SHOWN && <Text dimColor>{list.length - SHOWN} more waiting</Text>}
      </Box>
    )
  })
}
