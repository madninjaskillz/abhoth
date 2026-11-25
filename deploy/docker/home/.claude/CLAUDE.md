# Progress bars

For any task likely to take longer than about a minute, show a progress bar:

- start: `node ~/.claude/progress.js set <key> 0 "<label>"`
- update at meaningful milestones: `node ~/.claude/progress.js set <key> <pct> "<label>"`
- finish (success, failure, or stopping to ask a question): `node ~/.claude/progress.js done <key>`

`<key>` is a short unique id per task; the label is at most four words; the percentage is an
honest estimate of the whole task. Never run `progress.js clear`.
