/** One way to answer a block: an option of the sbx approval, or sand's own. */
export type Choice = { id: string; label: string; isDeny: boolean }

/**
 * A host the sandbox could not reach, or a host directory the agent asked to
 * mount (`mount`, its path in `host`). `approval` is the sbx approval id when
 * the daemon published one; without it, allowing adds a rule for this sandbox.
 */
export type Block = {
  id: string
  host: string
  approval?: string
  choices: Choice[]
  status: 'open' | 'sent'
  /** A deny rule blocks it, which allowing lifts for this sandbox. */
  isRuled?: boolean
  /** A mount the agent asked for, with its words on why. */
  mount?: { reason?: string }
}

declare module 'claude-code' {
  interface PluginState {
    'sand': { blocks: Shaped<Block[]> }
  }
}
