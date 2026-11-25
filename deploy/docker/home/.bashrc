# Abhoth web terminal (written once; edit freely).
[[ -f /etc/bash.bashrc ]] && . /etc/bash.bashrc
PS1='\[\e[36m\]\w\[\e[0m\] \$ '
if [[ ! -s "$HOME/.claude/.credentials.json" && -z "${ANTHROPIC_API_KEY:-}" ]] || [[ ! -s "$HOME/.codex/auth.json" ]]; then
  echo "Bridge engines: sign in once here (saved in $HOME, kept across rebuilds):"
  [[ ! -s "$HOME/.claude/.credentials.json" ]] && echo "  claude auth login        # opens a URL; paste the code back here"
  [[ ! -s "$HOME/.codex/auth.json" ]] && echo "  codex login --device-auth   # shows a URL and a one-time code"
fi
