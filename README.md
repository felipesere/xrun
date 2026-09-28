# xrun

Run one command in every subdirectory of the current directory and show the
results as a table. No config file, no VCS awareness: it just iterates over
folders. Built for a directory full of jj or git checkouts, but it does not
care what is inside them.

```sh
cd ~/work            # contains api/, web/, infra/, ...
xrun -- jj st
```

```
 Dir   ┆ Output                                                    ┆ Exit
═══════╪═══════════════════════════════════════════════════════════╪══════
 api   ┆ The working copy has no changes.                          ┆ 0
       ┆ Working copy  (@) : qlwxqvqm eda550b9 (empty) (no desc…)  ┆
╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌
 web   ┆ Working copy changes:                                     ┆ 0
       ┆ M src/App.tsx                                             ┆
```

Install with `cargo install --path .`.

## How commands are run

- **One argument** is run through `sh -c`, so pipes, redirects and variables
  work: `xrun -- 'jj log -r "trunk()..@" | head -3'`
- **Several arguments** are run directly, which keeps quoting simple:
  `xrun -- claude -p "summarise this repo"`
- The directory name is exported as `$XRUN_DIR`.
- Stdin is closed. A command that needs input should redirect from a file.
- Stdout and stderr are merged into the Output column, ANSI codes stripped.
- The Exit column shows the exit code, or `timeout`, `signal`, or `error` when
  the program could not be started. xrun exits 1 if any directory failed.
- Each job runs in its own process group. A timeout or Ctrl-C kills the whole
  tree, including anything the shell or the command forked.

## Flags, each with a practical example

### `-j, --jobs N` limit concurrency

By default every directory runs at once. That is fine for `jj st` across
thirty repos, but not for anything that hits the network or an API quota.

```sh
xrun -j 4 -- jj git fetch                       # be polite to the remote
xrun -j 2 -- 'claude -p "..." < ../prompt.md'   # two Claude sessions at a time
xrun -j 1 -- cargo build                        # sequential, one build's worth of CPU
```

Rows always come out in directory order however the jobs finish.

### `-t, --timeout DURATION` kill runaway commands

Accepts plain seconds or a suffix: `90`, `90s`, `10m`, `1h`. The whole
process tree is killed and the row is marked `timeout`.

```sh
xrun -t 30s -- jj git fetch                     # a hung remote should not block the table
xrun -j 2 -t 15m -- 'claude -p --permission-mode acceptEdits < ../prompt.md'
```

Headless Claude can wait up to ten minutes for background work it started, so
a timeout is the backstop that guarantees you get your table back.

### `-f, --follow` watch output as it happens

Table mode buffers everything until the last job is done. For anything that
runs longer than a few seconds, `--follow` echoes each output line to stderr
as it arrives, prefixed with the directory. The table is still printed at the
end.

```sh
xrun -f -- cargo test
xrun -f -j 2 -t 15m -- 'claude -p --permission-mode acceptEdits < ../prompt.md'
```

```
api   │    Compiling api v0.3.0
web   │    Compiling web v1.2.0
api   │ test result: ok. 41 passed
web   │ test result: FAILED. 1 failed
```

### `--ignore NAME,...` skip directories

Comma separated, may be repeated. Matches directory names exactly.

```sh
xrun --ignore archive,scratch -- jj st
xrun --ignore archive --ignore scratch -- jj st
```

### `--only NAME,...` run in just these directories

The inverse of `--ignore`, and the two cannot be combined. Comma separated,
may be repeated. Matches directory names exactly. A named directory runs even
if it is hidden, and a name that matches nothing prints a warning.

```sh
xrun --only api,web -- cargo test
xrun --only api --only web -- cargo test
```

### `--omit-empty` show only directories with something to say

Hides rows that exited zero and printed nothing. Ideal for status checks
where most repos are clean.

```sh
xrun --omit-empty -- 'jj log -r "trunk()..@ & mine()" --no-graph -T "change_id.short() ++ \" \" ++ description.first_line() ++ \"\n\""'
xrun --omit-empty -- 'grep -l TODO README.md'
```

If nothing is left, xrun prints a one-line note instead of an empty table.

### `--stream` run sequentially in your terminal

Instead of building a table, run one directory at a time with your own
stdin, stdout, and stderr. Use it for interactive tools, or when you want
raw, unbuffered, unstripped output.

```sh
xrun --stream -- claude                         # interactive session per repo
xrun --stream -- jj log --limit 5               # colours preserved
```

### `--hidden` include dot-directories

```sh
xrun --hidden -- du -sh .
```

### `-n, --no-shell` never use the shell

Only matters when the command is a single argument that must not be
interpreted, such as a program whose path contains a space.

```sh
xrun -n -- '/Applications/Some Tool.app/Contents/MacOS/tool'
```

### `-q, --quiet` no progress lines

When stderr is a terminal, xrun prints `▶ name` when a job starts and
`✓ name 12.3s` or `✗ name 1 12.3s` when it ends. `-q` turns that off, for
example when capturing stderr in a script.

```sh
xrun -q -- jj st 2> errors.log
```

## Recipes

```sh
# Which repos have unpushed work?
xrun --omit-empty -- 'jj log -r "trunk()..@ & ~empty()" --no-graph -T "change_id.short() ++ \"\n\""'

# Fetch everything, four at a time, give up on stragglers
xrun -j 4 -t 1m -- jj git fetch

# Apply a prompt to every repo with Claude, watching progress, bounded to 15 minutes each
xrun -j 2 -f -t 15m -- 'claude -p --permission-mode acceptEdits \
  --allowedTools "Bash(yq *)" "Bash(grep *)" "Bash(jj diff *)" < ../prompt.md'

# Name the directory in the output
xrun -- 'echo "$XRUN_DIR: $(jj log -r @ --no-graph -T "change_id.short()")"'
```
