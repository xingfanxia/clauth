// The clauth mod's `$.state` contract: values the desk row's drawing reads
// and the event sequence counter, each held by the host so a hot reload of the
// module keeps them.
export type ClauthHeldRow = {
  /** The ask id the held check was posted under. */
  ask: string
  /** Which row line 2 shows: a permission ask's Allow/Deny, or an
   *  AskUserQuestion's answer-here. */
  kind: 'decision' | 'answers'
  /** The tool name, shown as line 1's `<Tool>`. */
  tool: string
  /** What the tool acts on (Bash's command, a path, a question, or one-line
   *  JSON), cut to the band's width at draw time. */
  desc: string
}

declare module 'claude-code' {
  interface PluginState {
    clauth: {
      /** The highest sequence number already sent on the bridge; the next
       *  event sends `seq + 1`, so a module reload never re-sends an old one. */
      seq: number
      /** The desk row shown above the prompt while a check is held, null when
       *  nothing is held. */
      heldRow: ClauthHeldRow | null
    }
  }
}
