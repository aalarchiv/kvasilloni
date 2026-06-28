# Agent Instructions

> **AI-FIRST NOTE - DLL export coverage when retargeting a new app**
>
> The shim's `canlib32.dll` exports only the **13 CANlib functions** the *current*
> target app (czone `configuration-tool.exe`) resolves. It is **NOT** a general
> replacement for the full Kvaser `canlib32.dll` (which exports hundreds).
>
> **Before pointing kvasilloni at a different application you MUST run an import
> coverage check** - enumerate the canlib symbols the new app resolves and diff
> them against what the shim exports. Any symbol the app imports that the shim
> does not export will fail `GetProcAddress`/load and break that app.
>
> Procedure:
> 1. List the new app's canlib imports (static import table *and* any dynamic
>    `GetProcAddress` calls):
>    ```bash
>    # static imports of canlib32.dll referenced by the app:
>    objdump -x THE_APP.exe | grep -iA40 'canlib32.dll'
>    # dynamic resolution - scan for canXxx / kvXxx symbol strings the app may
>    # pass to GetProcAddress (static table won't show these):
>    strings -e l THE_APP.exe | grep -E '^(can|kv)[A-Z]'
>    strings    THE_APP.exe | grep -E '^(can|kv)[A-Z]'
>    ```
>    (Under wine, `KVASILLONI_LOG` also records every resolved call at runtime -
>    use it to catch dynamically-resolved symbols the static scan misses.)
> 2. Diff that set against the shim's exports:
>    ```bash
>    make verify   # lists the 13 exported names
>    ```
> 3. For each missing symbol, implement it in `src/lib.rs` (+ add to the
>    coverage/selftest) before declaring the new app supported. Update the export
>    tables in `docs/RETARGETING.md` with the new export set.
>
> See `bd memories canlib` for the recorded rationale.

This project uses **bd** (beads) for issue tracking. Run `bd prime` for full workflow context.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
bd dolt push          # Push beads data to remote
```

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking - do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge - do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
