/*
 * Temporary Sandbox → Box alias shims for PR #706 (refactor: rename
 * Sandbox → Box).
 *
 * The api/ rename in #706 already updated its import sites to the
 * Box-prefixed identifiers, but the auto-generated runner-api-client
 * cannot be regenerated until the runner OpenAPI doc is renamed
 * (Part 2 of the WIP series). Until then, this file re-exports the
 * still-Sandbox-named generated symbols under their Box-prefixed
 * aliases so the API compiles and runs against the current
 * generated client.
 *
 * Delete this file (and the matching re-exports in index.ts) once
 * Part 2 lands and the client is regenerated with Box-named symbols.
 */

export { SandboxApi as BoxApi } from './api/sandbox-api'
export type { CreateSandboxDTO as CreateBoxDTO } from './models/create-sandbox-dto'
export type { RecoverSandboxDTO as RecoverBoxDTO } from './models/recover-sandbox-dto'

// `EnumsSandboxState` is both a runtime const (object literal) and a
// type alias; re-export both shapes so consumers can use it as a
// value (e.g. `EnumsBoxState.STARTED`) and as a type
// (`x: EnumsBoxState`).
export { EnumsSandboxState as EnumsBoxState } from './models/enums-sandbox-state'
export type { EnumsSandboxState as EnumsBoxStateType } from './models/enums-sandbox-state'
