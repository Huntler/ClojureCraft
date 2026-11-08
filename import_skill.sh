#!/usr/bin/env bash
# Installs the ClojureCraft skill into the user's global Claude Code configuration, so Claude
# uses it in every project, not only in this repository.
#
# Usage: ./import_skill.sh [--link] [--force]
#   --link   symlink the skill instead of copying it (follows updates of this repository)
#   --force  replace an existing installation
#
# The destination is $CLAUDE_CONFIG_DIR/skills/clojurecraft (default ~/.claude/skills/clojurecraft).

set -euo pipefail

link=false
force=false
for arg in "$@"; do
    case "$arg" in
        --link) link=true ;;
        --force) force=true ;;
        -h | --help)
            sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "Unknown option: $arg (see --help)" >&2
            exit 2
            ;;
    esac
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source="$repo/.claude/skills/clojurecraft"
config="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
target="$config/skills/clojurecraft"

if [[ ! -f "$source/SKILL.md" ]]; then
    echo "Skill not found at $source" >&2
    exit 1
fi

if [[ -e "$target" || -L "$target" ]]; then
    if [[ "$force" != true ]]; then
        echo "A skill is already installed at $target. Use --force to replace it." >&2
        exit 1
    fi
    rm -rf "$target"
fi

mkdir -p "$config/skills"
if [[ "$link" == true ]]; then
    ln -s "$source" "$target"
    echo "Linked $target -> $source"
else
    cp -R "$source" "$target"
    echo "Copied the skill to $target"
fi

cat <<EOF

The skill uses the ClojureCraft MCP server. To make the server available in every project:

  cargo build --manifest-path "$repo/Cargo.toml" -p clojurecraft-app
  claude mcp add --scope user clojurecraft -- "$repo/target/debug/clojurecraft" mcp
EOF
