#!/usr/bin/env bash
# Update the boxlite-runner binary on one or all runner EC2s via SSM.
#
# Usage:
#   ./runner-update-binary.sh                        # version from Cargo.toml
#   ./runner-update-binary.sh 0.9.6                  # explicit release version
#   ./runner-update-binary.sh --commit abc1234       # unreleased commit (no GitHub Release)
#   ./runner-update-binary.sh --instance i-xxx ...   # target specific instance(s)
#
# For released versions the SSM command downloads directly from GitHub Releases.
# For unreleased commits the artifact is fetched locally via `gh`, staged to S3,
# and the SSM command pulls from a short-lived presigned URL — no permanent S3
# bucket needed beyond the existing deployment bucket SST provisions.
#
# Requirements:
#   aws   — AWS CLI v2, credentials with SSM + EC2 + S3 access
#   gh    -- GitHub CLI, authenticated (only for --commit)
#   jq    — JSON parsing
#
# Environment:
#   STAGE              SST stage (default: production)
#   GH_REPO            GitHub repo slug (default: boxlite-ai/boxlite)
#   S3_STAGING_BUCKET  S3 bucket for staging unreleased binaries (default: auto-detect from SST)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
GH_REPO="${GH_REPO:-boxlite-ai/boxlite}"
STAGE="${STAGE:-production}"

# ── Argument parsing ──────────────────────────────────────────────────────────

MODE="release"      # release | commit
VERSION=""
COMMIT_SHA=""
INSTANCE_IDS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --commit)
      MODE="commit"
      COMMIT_SHA="${2:?--commit requires a SHA}"
      shift 2
      ;;
    --instance)
      INSTANCE_IDS+=("${2:?--instance requires an instance ID}")
      shift 2
      ;;
    --help|-h)
      sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | grep '^#' | sed 's/^# \?//'
      exit 0
      ;;
    -*)
      echo "Unknown option: $1" >&2; exit 1
      ;;
    *)
      VERSION="$1"
      shift
      ;;
  esac
done

# ── Resolve version (release mode) ───────────────────────────────────────────

if [[ "$MODE" == "release" && -z "$VERSION" ]]; then
  VERSION=$(grep -m1 '^version\s*=' "${REPO_ROOT}/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')
  echo "→ version from Cargo.toml: ${VERSION}"
fi

# ── Discover runner instances (if not explicit) ───────────────────────────────

if [[ ${#INSTANCE_IDS[@]} -eq 0 ]]; then
  echo "→ discovering runner instances tagged Name=boxlite-runner*…"
  mapfile -t INSTANCE_IDS < <(
    aws ec2 describe-instances \
      --filters \
        "Name=tag:Name,Values=boxlite-runner,boxlite-runner-*" \
        "Name=instance-state-name,Values=running" \
      --query 'Reservations[].Instances[].InstanceId' \
      --output text | tr '\t' '\n'
  )
fi

if [[ ${#INSTANCE_IDS[@]} -eq 0 ]]; then
  echo "No running runner instances found." >&2
  exit 1
fi

echo "→ target instances: ${INSTANCE_IDS[*]}"

# ── Build the remote install script ──────────────────────────────────────────

build_release_commands() {
  local ver="$1"
  cat <<REMOTE
set -euo pipefail
ARCHIVE="boxlite-runner-v${ver}-linux-amd64.tar.gz"
URL="https://github.com/${GH_REPO}/releases/download/v${ver}/\${ARCHIVE}"
echo "Downloading \${URL}…"
curl -fsSL "\${URL}" -o /tmp/\${ARCHIVE}
systemctl stop boxlite-runner
tar xz -C /usr/local/bin/ -f /tmp/\${ARCHIVE}
rm /tmp/\${ARCHIVE}
systemctl start boxlite-runner
echo "Runner updated to v${ver} — \$(boxlite-runner --version 2>&1 || true)"
REMOTE
}

build_s3_commands() {
  local presigned_url="$1"
  local short_sha="$2"
  cat <<REMOTE
set -euo pipefail
echo "Downloading runner artifact for commit ${short_sha}…"
curl -fsSL "${presigned_url}" -o /tmp/boxlite-runner-artifact.tar.gz
systemctl stop boxlite-runner
tar xz -C /usr/local/bin/ -f /tmp/boxlite-runner-artifact.tar.gz
rm /tmp/boxlite-runner-artifact.tar.gz
systemctl start boxlite-runner
echo "Runner updated to commit ${short_sha} — \$(boxlite-runner --version 2>&1 || true)"
REMOTE
}

# ── Commit mode: stage artifact via S3 ───────────────────────────────────────

PRESIGNED_URL=""
SHORT_SHA=""

if [[ "$MODE" == "commit" ]]; then
  SHORT_SHA="${COMMIT_SHA:0:7}"
  echo "→ commit mode: ${COMMIT_SHA} (${SHORT_SHA})"

  # Find the GitHub Actions run for this commit
  echo "→ finding Actions run for commit…"
  RUN_ID=$(gh run list \
    --repo "${GH_REPO}" \
    --commit "${COMMIT_SHA}" \
    --workflow "Build Runner Binary" \
    --status success \
    --limit 1 \
    --json databaseId \
    --jq '.[0].databaseId')

  if [[ -z "$RUN_ID" || "$RUN_ID" == "null" ]]; then
    echo "No successful 'Build Runner Binary' run found for commit ${COMMIT_SHA}." >&2
    echo "Check: gh run list --repo ${GH_REPO} --commit ${COMMIT_SHA}" >&2
    exit 1
  fi

  echo "→ Actions run: ${RUN_ID}"

  # Download artifact locally
  ARTIFACT_DIR=$(mktemp -d)
  trap 'rm -rf "${ARTIFACT_DIR}"' EXIT

  echo "→ downloading artifact runner-linux-amd64…"
  gh run download "${RUN_ID}" \
    --repo "${GH_REPO}" \
    --name runner-linux-amd64 \
    --dir "${ARTIFACT_DIR}"

  ARTIFACT_TAR=$(find "${ARTIFACT_DIR}" -name "*.tar.gz" | head -1)
  if [[ -z "$ARTIFACT_TAR" ]]; then
    echo "No .tar.gz found in downloaded artifact." >&2
    exit 1
  fi

  # Resolve S3 staging bucket
  if [[ -z "${S3_STAGING_BUCKET:-}" ]]; then
    echo "→ resolving S3 staging bucket from SST (stage: ${STAGE})…"
    S3_STAGING_BUCKET=$(npx --yes sst shell --stage "${STAGE}" -- \
      bash -c 'echo $REGISTRY_BUCKET' 2>/dev/null || true)
  fi

  if [[ -z "${S3_STAGING_BUCKET:-}" ]]; then
    echo "S3_STAGING_BUCKET is not set and could not be auto-detected." >&2
    echo "Set it explicitly: S3_STAGING_BUCKET=my-bucket ./runner-update-binary.sh --commit ${COMMIT_SHA}" >&2
    exit 1
  fi

  S3_KEY="runner-staging/${SHORT_SHA}/boxlite-runner-linux-amd64.tar.gz"
  echo "→ uploading to s3://${S3_STAGING_BUCKET}/${S3_KEY}…"
  aws s3 cp "${ARTIFACT_TAR}" "s3://${S3_STAGING_BUCKET}/${S3_KEY}"

  # Presign for 1 hour — enough time for SSM to pull on all instances
  PRESIGNED_URL=$(aws s3 presign "s3://${S3_STAGING_BUCKET}/${S3_KEY}" \
    --expires-in 3600)
  echo "→ presigned URL generated (valid 1 h)"
fi

# ── Send SSM command ──────────────────────────────────────────────────────────

if [[ "$MODE" == "release" ]]; then
  REMOTE_COMMANDS=$(build_release_commands "${VERSION}")
  DESCRIPTION="Update boxlite-runner to v${VERSION}"
else
  REMOTE_COMMANDS=$(build_s3_commands "${PRESIGNED_URL}" "${SHORT_SHA}")
  DESCRIPTION="Update boxlite-runner to commit ${SHORT_SHA}"
fi

echo "→ sending SSM Run Command: ${DESCRIPTION}…"
COMMAND_ID=$(aws ssm send-command \
  --instance-ids "${INSTANCE_IDS[@]}" \
  --document-name "AWS-RunShellScript" \
  --comment "${DESCRIPTION}" \
  --parameters "commands=[$(echo "${REMOTE_COMMANDS}" | jq -Rsa . | sed 's/^"\(.*\)"$/\1/')]" \
  --query 'Command.CommandId' \
  --output text)

echo "→ command ID: ${COMMAND_ID}"
echo "→ waiting for completion…"

# Poll until all invocations finish
ALL_OK=true
for INSTANCE_ID in "${INSTANCE_IDS[@]}"; do
  echo -n "  ${INSTANCE_ID}: "
  aws ssm wait command-executed \
    --command-id "${COMMAND_ID}" \
    --instance-id "${INSTANCE_ID}" 2>/dev/null || true

  STATUS=$(aws ssm get-command-invocation \
    --command-id "${COMMAND_ID}" \
    --instance-id "${INSTANCE_ID}" \
    --query 'Status' \
    --output text)

  if [[ "$STATUS" == "Success" ]]; then
    echo "✓"
  else
    echo "✗ (${STATUS})"
    aws ssm get-command-invocation \
      --command-id "${COMMAND_ID}" \
      --instance-id "${INSTANCE_ID}" \
      --query 'StandardErrorContent' \
      --output text >&2
    ALL_OK=false
  fi
done

# Clean up S3 staging object (best-effort)
if [[ "$MODE" == "commit" && -n "${S3_KEY:-}" ]]; then
  echo "→ cleaning up staging object…"
  aws s3 rm "s3://${S3_STAGING_BUCKET}/${S3_KEY}" || true
fi

if [[ "$ALL_OK" == "true" ]]; then
  echo "Done."
else
  echo "One or more instances failed. Check SSM console for details." >&2
  exit 1
fi
