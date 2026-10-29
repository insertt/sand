import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const BAND = {
  plugin: 'sand',
  component: 'AbovePrompt',
  props: { hasSurvey: false, isWorking: false, maxRows: 20, bodyColumns: 100, scroll: { offset: 0, bodyRows: 20 }, view: {} },
} as const
const DIR = '/home/agent/.sand-policy'
const BLOCKED = 'Blocked by network policy: domain example.com:443\n  detail: no matching allow rule'

/** The bridge directory as files in memory; what the mod writes lands there too. */
const bridge = (on: On, files: Record<string, string>, toasts: string[] = [], notes: string[] = []) => {
  mock.env(on, { HOME: '/home/agent', SANDBOX_NAME: 'claude-proj' })
  on('ui.notify', (_, e) => {
    notes.push(e.text)
    return { value: { isSent: true, channel: 'iterm2' } }
  })
  on('ui.toast', (_, e) => {
    toasts.push(e.text)
    return { value: undefined }
  })
  // The engine's own band: nothing.
  on('ui.render', ($, e) => {
    const { Text } = $.ui.resolve(e)
    return <Text>engine</Text>
  })
  on('fs.read', (_, e) =>
    e.path in files ? { value: files[e.path] as string } : Promise.reject(new Error(`ENOENT: ${e.path}`)),
  )
  on('fs.write', (_, e) => {
    files[e.path] = e.text
    return { value: undefined }
  })
  return files
}

test('a blocked host gets a row, the model is told to wait, and Allow asks sand', async ($, on) => {
  const notes: string[] = []
  const files = bridge(on, {}, [], notes)
  on('tool.call', () => ({ result: {}, text: BLOCKED }))

  const ran = await $.tool.call({ tool: 'Bash', command: 'curl https://example.com' })
  expect(ran.context?.[0]).toMatch(/being asked whether to allow example.com:443/)

  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
  expect((await ui.find({ type: 'Text', text: /example.com:443/ }))?.text).toBe('example.com:443')
  const allow = (await ui.findAll({ type: 'Button' })).find(b => b.props.label === 'Allow for this sandbox')
  await ui.press({ key: allow?.key as string })

  expect(notes).toEqual(['claude-proj is blocked from reaching example.com:443. Answer in Claude Code.'])
  const [path, request] = Object.entries(files).find(([p]) => p.includes('/requests/')) ?? []
  expect(path).toMatch(new RegExp(`^${DIR}/requests/[0-9a-f-]+\\.json$`))
  expect(JSON.parse(request as string)).toEqual({ host: 'example.com:443', approval: null, option: null })
  expect(await ui.find({ type: 'Text', text: /confirm on the host/ })).toBeDefined()
  await ui.unmount()
})

test('a host an earlier refusal blocks can be allowed again', async ($, on) => {
  const files = bridge(on, {})
  on('tool.call', () => ({ result: {}, text: 'HTTP/1.1 403 Forbidden\nBlocked by local rule for example.com:443' }))

  const ran = await $.tool.call({ tool: 'Bash', command: 'curl https://example.com' })
  expect(ran.context?.[0]).toMatch(/being asked whether to allow example.com:443/)

  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
  expect(await ui.find({ type: 'Text', text: /deny rule/ })).toBeDefined()
  const lift = (await ui.findAll({ type: 'Button' })).find(b => b.props.label === 'Lift the deny rule and allow')
  await ui.press({ key: lift?.key as string })
  const request = Object.entries(files).find(([p]) => p.includes('/requests/'))?.[1]
  expect(JSON.parse(request as string)).toEqual({ host: 'example.com:443', approval: null, option: null })
  await ui.unmount()
})

test('an sbx approval offers its own options, and a result clears the row and tells the model', async ($, on) => {
  const clock = mock.clock(on)
  const session = mock.session(on)
  const pending = { approvals: [{ id: 'ap-1', host: 'pypi.org', options: [{ id: 'once', label: 'Allow once' }, { id: 'deny', label: 'Deny' }] }] }
  const toasts: string[] = []
  const files = bridge(on, { [`${DIR}/pending.json`]: JSON.stringify(pending) }, toasts)
  on('session.start', (_, e) => ({ cwd: e.cwd }))

  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await clock.advance(1600)
  await clock.settle()

  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
  const labels = (await ui.findAll({ type: 'Button' })).map(b => b.props.label)
  expect(labels).toEqual(['Allow once', 'Deny'])
  const once = (await ui.findAll({ type: 'Button' })).find(b => b.props.label === 'Allow once')
  await ui.press({ key: once?.key as string })

  const [path, request] = Object.entries(files).find(([p]) => p.includes('/requests/')) ?? []
  expect(JSON.parse(request as string)).toEqual({ host: 'pypi.org', approval: 'ap-1', option: 'once' })
  const id = (path as string).split('/').pop()
  files[`${DIR}/results/${id}`] = JSON.stringify({ ok: true, allowed: true, message: 'pypi.org allowed' })
  await clock.advance(1600)
  await clock.settle()

  expect(await ui.find({ type: 'Text', text: /pypi.org/ })).toBeUndefined()
  expect(toasts.at(-1)).toBe('pypi.org allowed')
  const note = JSON.stringify(session.appended().at(-1)?.message)
  expect(note).toMatch(/allowed pypi.org/)
  await ui.unmount()
})

test('the agent asks to mount a directory, the user picks read-only, and the result reaches the model', async ($, on) => {
  const clock = mock.clock(on)
  const session = mock.session(on)
  const files = bridge(on, {})
  const registered: string[] = []
  on('tool.register', (_, e) => {
    registered.push(e.name)
    return { value: { tool: `mcp__sand__${e.name}` } }
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  expect(registered).toEqual(['request_mount'])

  // The command line's syntax: the suffix names the mode.
  const ran = await $.tool.call({ tool: 'mcp__sand__request_mount', path: '~/notes:ro', reason: 'the user asked' })
  // The engine takes a plugin tool's result as text (or content blocks), never an object.
  expect(typeof ran.result).toBe('string')
  expect(ran.result).toMatch(/Asked the user to mount ~\/notes \(ro\)/)
  const empty = await $.tool.call({ tool: 'mcp__sand__request_mount', path: ' ' })
  expect(empty.deny).toBe('Give the host directory as `path`.')

  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
  expect(await ui.find({ type: 'Text', text: /asks to mount this host directory: the user asked/ })).toBeDefined()
  const buttons = await ui.findAll({ type: 'Button' })
  expect(buttons.map(b => b.props.label)).toEqual(['Mount read-only', 'Mount read-write', 'Dismiss'])
  await ui.press({ key: buttons[0]?.key as string })

  const [path, request] = Object.entries(files).find(([p]) => p.includes('/requests/')) ?? []
  expect(JSON.parse(request as string)).toEqual({ kind: 'mount', path: '~/notes', mode: 'ro', reason: 'the user asked' })
  const id = (path as string).split('/').pop()
  files[`${DIR}/results/${id}`] = JSON.stringify({
    ok: true,
    allowed: true,
    message: 'mounted /Users/me/notes (ro) at /Users/me/notes',
  })
  await clock.advance(1600)
  await clock.settle()

  const note = JSON.stringify(session.appended().at(-1)?.message)
  expect(note).toMatch(/sand mounted \/Users\/me\/notes \(ro\) at \/Users\/me\/notes; it is available/)
  await ui.unmount()
})
