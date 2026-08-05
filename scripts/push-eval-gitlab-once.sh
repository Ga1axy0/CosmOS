#!/usr/bin/env bash

set -euo pipefail

# One-time evaluation export.
#
# The source repository is never flattened.  A temporary Git repository is
# populated from local Git objects, flattened, committed, and pushed to GitLab.
# No source or submodule repository is cloned from the network.

SOURCE_ROOT="$(git rev-parse --show-toplevel)"
SOURCE_COMMIT="$(git -C "$SOURCE_ROOT" rev-parse --verify HEAD)"
SOURCE_GIT_COMMON="$(git -C "$SOURCE_ROOT" rev-parse --git-common-dir)"
if [[ "$SOURCE_GIT_COMMON" != /* ]]; then
    SOURCE_GIT_COMMON="$SOURCE_ROOT/$SOURCE_GIT_COMMON"
fi
SOURCE_GIT_COMMON="$(cd -- "$SOURCE_GIT_COMMON" && pwd -P)"
GITLAB_URL="${GITLAB_MIRROR_URL:-https://oauth2@gitlab.eduxiji.net/T2026103369910596/cosmos.git}"
TARGET_BRANCH="${GITLAB_TARGET_BRANCH:-main}"
TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/cosmos-gitlab-export.XXXXXX")"
TEMP_REPO="$TEMP_ROOT/repo"

cleanup() {
    rm -rf -- "$TEMP_ROOT"
}
trap cleanup EXIT

if [[ -n "$(git -C "$SOURCE_ROOT" status --porcelain=v1)" ]]; then
    echo "[WARN] The source worktree has uncommitted changes." >&2
    echo "[WARN] Only committed HEAD $SOURCE_COMMIT will be exported." >&2
fi

archive_local_repository() {
    local repo_git_dir="$1"
    local commit="$2"
    local destination="$3"
    local modules_file
    local entries_file

    if [[ ! -d "$repo_git_dir" ]]; then
        echo "[ERROR] Local submodule repository is missing: $repo_git_dir" >&2
        echo "[ERROR] Initialize that submodule locally before running this script." >&2
        exit 1
    fi

    mkdir -p -- "$destination"
    git --git-dir="$repo_git_dir" archive --format=tar "$commit" \
        | tar -xf - -C "$destination"

    # Recursively replace gitlinks with the corresponding local submodule
    # trees.  A submodule gitdir is stored below its parent's modules/ folder.
    if ! git --git-dir="$repo_git_dir" cat-file -e "$commit:.gitmodules" 2>/dev/null; then
        return
    fi

    modules_file="$(mktemp "$TEMP_ROOT/modules.XXXXXX")"
    entries_file="$(mktemp "$TEMP_ROOT/entries.XXXXXX")"
    git --git-dir="$repo_git_dir" show "$commit:.gitmodules" > "$modules_file"

    if git --git-dir="$repo_git_dir" config --file "$modules_file" \
        --get-regexp '^submodule\..*\.path$' > "$entries_file"; then
        while IFS=' ' read -r submodule_key submodule_path; do
            [[ -n "$submodule_path" ]] || continue

            submodule_name="${submodule_key#submodule.}"
            submodule_name="${submodule_name%.path}"

            case "$submodule_path" in
                /*|..|../*|*/../*)
                    echo "[ERROR] Unsafe submodule path: $submodule_path" >&2
                    exit 1
                    ;;
            esac

            submodule_commit="$(
                git --git-dir="$repo_git_dir" ls-tree "$commit" -- "$submodule_path" \
                    | awk '$1 == "160000" { print $3; exit }'
            )"
            if [[ -z "$submodule_commit" ]]; then
                echo "[ERROR] Cannot find gitlink for $submodule_path at $commit" >&2
                exit 1
            fi

            echo "[INFO] Expanding local submodule: $destination/$submodule_path"
            archive_local_repository \
                "$repo_git_dir/modules/$submodule_name" \
                "$submodule_commit" \
                "$destination/$submodule_path"
        done < "$entries_file"
    fi

    rm -f -- "$modules_file" "$entries_file"
}

echo "[INFO] Creating a temporary repository from local Git records"
git init --quiet "$TEMP_REPO"
git -C "$SOURCE_ROOT" archive --format=tar "$SOURCE_COMMIT" \
    | tar -xf - -C "$TEMP_REPO"

SOURCE_MODULES_FILE="$(mktemp "$TEMP_ROOT/source-modules.XXXXXX")"
SOURCE_ENTRIES_FILE="$(mktemp "$TEMP_ROOT/source-entries.XXXXXX")"
if git -C "$SOURCE_ROOT" cat-file -e "$SOURCE_COMMIT:.gitmodules"; then
    git -C "$SOURCE_ROOT" show "$SOURCE_COMMIT:.gitmodules" > "$SOURCE_MODULES_FILE"
    git -C "$SOURCE_ROOT" config --file "$SOURCE_MODULES_FILE" \
        --get-regexp '^submodule\..*\.path$' > "$SOURCE_ENTRIES_FILE"

    while IFS=' ' read -r submodule_key submodule_path; do
        [[ -n "$submodule_path" ]] || continue

        submodule_name="${submodule_key#submodule.}"
        submodule_name="${submodule_name%.path}"

        case "$submodule_path" in
            /*|..|../*|*/../*)
                echo "[ERROR] Unsafe submodule path: $submodule_path" >&2
                exit 1
                ;;
        esac

        submodule_commit="$(
            git -C "$SOURCE_ROOT" ls-tree "$SOURCE_COMMIT" -- "$submodule_path" \
                | awk '$1 == "160000" { print $3; exit }'
        )"
        if [[ -z "$submodule_commit" ]]; then
            echo "[ERROR] Cannot find gitlink for $submodule_path at $SOURCE_COMMIT" >&2
            exit 1
        fi

        archive_local_repository \
            "$SOURCE_GIT_COMMON/modules/$submodule_name" \
            "$submodule_commit" \
            "$TEMP_REPO/$submodule_path"
    done < "$SOURCE_ENTRIES_FILE"
fi
rm -f -- "$SOURCE_MODULES_FILE" "$SOURCE_ENTRIES_FILE"

# Remove these files only from the temporary export.  The originals in the
# source worktree remain untouched.
EXCLUDED_FILES=(
    "CosmOS-rootfs/third-party/gcc-11.2.0.tar.xz"
    "CosmOS-rootfs/third-party/gcc-13.2.0.tar.xz"
    "CosmOS-rootfs/third-party/libclang-riscv64-noble/libllvm18_18.1.3-1ubuntu1_riscv64.deb"
)

# Nested submodules are expanded by archive_local_repository() into the
# temporary export.  Keep TGOSKits in the submission because the rootfs build
# uses its Cargo.toml/Cargo.lock to prepare the offline cache; it is still
# excluded from disk.img/rootfs variants by WITH_TGOSKITS=0.
EXCLUDED_SUBMODULES=()

for path in "${EXCLUDED_FILES[@]}"; do
    rm -f -- "$TEMP_REPO/$path"
done
for path in "${EXCLUDED_SUBMODULES[@]}"; do
    rm -rf -- "$TEMP_REPO/$path"
done

# Remove all submodule metadata from the remote snapshot.  The source tree is
# untouched, including its original .gitmodules files and gitdir metadata.
find "$TEMP_REPO" -type f -name .gitmodules -delete

for path in "${EXCLUDED_FILES[@]}"; do
    if [[ -e "$TEMP_REPO/$path" ]]; then
        echo "[ERROR] Excluded file is still present in the export: $path" >&2
        exit 1
    fi
done
for path in "${EXCLUDED_SUBMODULES[@]}"; do
    if [[ -e "$TEMP_REPO/$path" ]]; then
        echo "[ERROR] Excluded submodule is still present in the export: $path" >&2
        exit 1
    fi
done

# The GitLab endpoint limits the size of each received pack to 50 MiB.  Keep
# each upload comfortably below that limit.  A single file larger than this
# value cannot be uploaded using ordinary Git pushes and must be handled
# separately (for example with Git LFS).
BATCH_MIB="${GITLAB_BATCH_MIB:-32}"
if ! [[ "$BATCH_MIB" =~ ^[1-9][0-9]*$ ]]; then
    echo "[ERROR] GITLAB_BATCH_MIB must be a positive integer." >&2
    exit 1
fi
BATCH_LIMIT=$((BATCH_MIB * 1024 * 1024))

# Force-add only the temporary archive-derived tree.  This is safe here: the
# temporary repository contains no source worktree or build artifacts.
git -C "$TEMP_REPO" add -A --force

for path in "${EXCLUDED_FILES[@]}"; do
    if git -C "$TEMP_REPO" ls-files --error-unmatch -- "$path" >/dev/null 2>&1; then
        echo "[ERROR] Excluded file is still tracked in the export: $path" >&2
        exit 1
    fi
done

if git -C "$TEMP_REPO" ls-files --stage | grep -q '^160000 '; then
    echo "[ERROR] The exported tree still contains a gitlink/submodule." >&2
    exit 1
fi

# Record blob sizes before clearing the index.  The size budget is deliberately
# based on uncompressed blob sizes, which leaves room for tree and commit
# objects in every received pack.
SIZES_FILE="$(mktemp "$TEMP_ROOT/file-sizes.XXXXXX")"
while IFS= read -r -d '' path; do
    blob_sha="$(git -C "$TEMP_REPO" ls-files --stage -- "$path" | awk '{print $2; exit}')"
    blob_size="$(git -C "$TEMP_REPO" cat-file -s "$blob_sha")"
    printf '%s\t%s\n' "$blob_size" "$path" >> "$SIZES_FILE"
done < <(git -C "$TEMP_REPO" ls-files -z)

# Create an empty index and build the final tree incrementally.  Each commit
# is pushed to a staging branch before the next commit is created, so the next
# push only contains the newly added batch's objects.
git -C "$TEMP_REPO" read-tree --empty
STAGING_BRANCH="mirror-upload-${SOURCE_COMMIT:0:12}"
PREVIOUS_COMMIT=""
BATCH_NUMBER=0
BATCH_BYTES=0
BATCH_PATHS=()

prepare_credentials() {
    if [[ -z "${GITLAB_TOKEN:-}" ]]; then
        if [[ ! -t 0 ]]; then
            echo "[ERROR] Set GITLAB_TOKEN when running non-interactively." >&2
            exit 1
        fi
        read -r -s -p "GitLab token: " GITLAB_TOKEN
        printf '\n'
    fi
    export GITLAB_TOKEN

    GIT_ASKPASS="$TEMP_ROOT/git-askpass"
    cat > "$GIT_ASKPASS" <<'EOF'
#!/usr/bin/env sh
case "${1:-}" in
    *Username*) printf '%s\n' 'oauth2' ;;
    *) printf '%s\n' "${GITLAB_TOKEN:?}" ;;
esac
EOF
    chmod 700 "$GIT_ASKPASS"
    export GIT_ASKPASS
    export GIT_TERMINAL_PROMPT=0
}

push_staging_commit() {
    local commit="$1"
    git -C "$TEMP_REPO" -c credential.helper= push --force \
        "$GITLAB_URL" "$commit:refs/heads/$STAGING_BRANCH"
}

push_current_batch() {
    local tree_sha
    local mirror_commit
    local parent_args=()

    if ((${#BATCH_PATHS[@]} == 0)); then
        return
    fi

    ((BATCH_NUMBER += 1))
    git -C "$TEMP_REPO" add -A --force -- "${BATCH_PATHS[@]}"
    tree_sha="$(git -C "$TEMP_REPO" write-tree)"
    if [[ -n "$PREVIOUS_COMMIT" ]]; then
        parent_args=(-p "$PREVIOUS_COMMIT")
    fi
    mirror_commit="$(
        git -C "$TEMP_REPO" \
            -c user.name='evaluation-mirror' \
            -c user.email='evaluation-mirror@localhost' \
            -c commit.gpgsign=false \
            commit-tree "$tree_sha" "${parent_args[@]}" \
            -m "Mirror evaluation batch $BATCH_NUMBER"
    )"

    echo "[INFO] Uploading batch $BATCH_NUMBER (${BATCH_BYTES} bytes)"
    push_staging_commit "$mirror_commit"
    PREVIOUS_COMMIT="$mirror_commit"
    BATCH_PATHS=()
    BATCH_BYTES=0
}

prepare_credentials

while IFS=$'\t' read -r blob_size path; do
    [[ -n "$path" ]] || continue

    if ((blob_size > BATCH_LIMIT)); then
        echo "[ERROR] A single file exceeds the batch limit: $blob_size bytes: $path" >&2
        exit 1
    fi
    if ((BATCH_BYTES > 0 && BATCH_BYTES + blob_size > BATCH_LIMIT)); then
        push_current_batch
    fi
    BATCH_PATHS+=("$path")
    BATCH_BYTES=$((BATCH_BYTES + blob_size))
done < "$SIZES_FILE"
push_current_batch

if [[ -z "$PREVIOUS_COMMIT" ]]; then
    echo "[ERROR] The export tree is empty; no snapshot was created." >&2
    exit 1
fi

echo "[INFO] Publishing the final snapshot to GitLab branch $TARGET_BRANCH"
git -C "$TEMP_REPO" -c credential.helper= push --force \
    "$GITLAB_URL" "$PREVIOUS_COMMIT:refs/heads/$TARGET_BRANCH"

echo "[INFO] Removing temporary staging branch"
git -C "$TEMP_REPO" -c credential.helper= push --force \
    "$GITLAB_URL" ":refs/heads/$STAGING_BRANCH" || \
    echo "[WARN] Could not remove staging branch $STAGING_BRANCH" >&2

echo "[OK] Evaluation snapshot pushed to GitLab."
