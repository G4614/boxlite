export const BOX_CREATE_SECRETS_TTL_SECONDS = 24 * 60 * 60

export function boxCreateSecretsKey(boxId: string): string {
  return `box:create-secrets:${boxId}`
}

// Transient marker: box was created with secret substitution pre-provisioned
// (per-box MITM CA) even when no create-time secrets were supplied. Carried
// from create until the reconcile that first creates the box on its runner,
// then deleted alongside the create-secrets key. Same lifecycle/TTL as secrets.
export function boxCreateSecretSubstitutionKey(boxId: string): string {
  return `box:create-secret-substitution:${boxId}`
}
