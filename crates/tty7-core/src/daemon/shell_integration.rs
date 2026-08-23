use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ZSH_INTEGRATION: &str = r#"
# --- tty7 shell integration (zsh) ---
if [[ -o interactive ]] && [[ -z "$TTY7_SHELL_INTEGRATION" ]]; then
  export TTY7_SHELL_INTEGRATION=1

  # Per-pane history, when the app asked for it. This runs after the user's
  # .zshrc, which is the only reason it can work at all: $HISTFILE is theirs to
  # set, wherever they like, and nothing outside this shell knew where it
  # points until now. Seed once from there so the pane does not start blank,
  # write down how much was seeded — everything past that mark is what this
  # pane adds, and is what goes home when it closes — and only then repoint.
  # zsh loads the history file after .zshrc, so the switch lands before the
  # first line is read.
  if [[ -n "$TTY7_HISTFILE" ]]; then
    if [[ ! -e "$TTY7_HISTFILE" ]]; then
      # The origin is recorded even when there is nothing to copy from it yet:
      # a first-ever shell has no history file, and it is exactly that user
      # whose commands would otherwise have nowhere to go when the pane closes.
      if [[ -n "$HISTFILE" ]]; then
        builtin printf '%s\n' "$HISTFILE" > "$TTY7_HISTFILE.origin" 2>/dev/null
        if [[ -r "$HISTFILE" ]]; then
          command tail -n 5000 "$HISTFILE" > "$TTY7_HISTFILE" 2>/dev/null
        fi
      fi
      [[ -e "$TTY7_HISTFILE" ]] || : > "$TTY7_HISTFILE" 2>/dev/null
      command wc -c < "$TTY7_HISTFILE" 2>/dev/null | command tr -d ' \n' \
        > "$TTY7_HISTFILE.seed" 2>/dev/null
    fi
    HISTFILE="$TTY7_HISTFILE"
    # The exit rewrite keeps at most SAVEHIST entries, from a memory capped at
    # HISTSIZE. Smaller than the seed, that rewrite leaves the pane's file
    # shorter than its own seed mark — which the merge on close reads as "the
    # file was replaced under us" and rightly refuses, losing this pane's
    # commands. The caps only govern the pane's private file now, so raising
    # them costs nothing. Left at zero: a user who saves no history has asked
    # for exactly that.
    if (( SAVEHIST > 0 )); then
      (( SAVEHIST < 100000 )) && SAVEHIST=100000
      (( HISTSIZE < SAVEHIST )) && HISTSIZE=$SAVEHIST
    fi
  fi

  __tty7_osc() { builtin printf '\e]%s\a' "$1"; }

  # Vi mode links the `main` keymap to `viins` (`bindkey -A viins main`);
  # emacs mode links it to `emacs`. The link survives plugins like
  # zsh-vi-mode that rebind `^[` to their own widgets, so it beats sniffing
  # the Esc widget name.
  __tty7_report_edit_mode() {
    if [[ "$(builtin bindkey -lL main)" == *viins* ]]; then
      __tty7_osc "133;V;1"
    else
      __tty7_osc "133;V;0"
    fi
  }

  # OSC 7: report the working directory so the app tracks it precisely (used for
  # opening new tabs / splits in the same place). The daemon percent-DECODES the
  # payload (OSC 7 carries a file: URI), so a literal `%` in the path must be
  # escaped as %25 or a dir like `/tmp/a%20b` would decode to `/tmp/a b`.
  __tty7_report_cwd() { builtin printf '\e]7;file://%s%s\a' "${HOST:-localhost}" "${PWD//\%/%25}"; }

  # D (command finished + its exit code) gets its own hook, *prepended* to
  # precmd_functions rather than bundled into __tty7_precmd below: the app only
  # switches back to prompt-editing when D arrives, so every hook that runs
  # before it is a window where keystrokes go raw to the PTY, get kernel-echoed
  # into the grid, and bait zsh's PROMPT_SP into leaving a stray `char + %` line.
  # The user's precmd chain (git-status prompts, conda, …) can take hundreds of
  # ms — D must not wait for it.
  __tty7_precmd_d() {
    local ret=$?
    if [[ -n "$__tty7_cmd_active" ]]; then
      __tty7_osc "133;D;$ret"
      unset __tty7_cmd_active
    fi
  }

  # The rest of the prompt bookkeeping runs right before the prompt is drawn,
  # *after* the user's hooks: report cwd, then open a fresh prompt (A).
  __tty7_precmd() {
    __tty7_report_cwd
    __tty7_report_edit_mode
    __tty7_osc "133;A"
    # Prompt-end marker (B): emitted at the very end of the prompt — exactly where
    # input begins — by living in PS1 (wrapped in %{...%} so zsh excludes it from
    # prompt width). We (re)append it here in precmd rather than once at load,
    # because prompt frameworks (powerlevel10k / starship / oh-my-zsh) rebuild
    # PS1 in their own precmd and would otherwise drop it. This precmd runs last
    # (added after the user's), and the sentinel check keeps a static PS1 from
    # accumulating duplicate markers.
    [[ "$PS1" != *$'\e]133;B\a'* ]] && PS1="$PS1"$'%{\e]133;B\a%}'
  }

  # preexec runs after the user hits Enter, before the command runs: mark the
  # start of command output (C). We track an "active" flag so the very first
  # prompt (no command yet) doesn't emit a bogus D. The C mark carries the
  # submitted line ($1), truncated (detection only reads the front) and with
  # the bytes that would break OSC framing or the daemon's percent-decode
  # escaped (% ESC BEL CR NL) — the coding-agent detection input on Windows,
  # where ConPTY has no process table to poll (see core::cli_agent).
  __tty7_preexec() {
    __tty7_cmd_active=1
    local cmd=$1
    cmd=${cmd[1,512]}
    cmd=${cmd//\%/%25}
    cmd=${cmd//$'\e'/%1B}
    cmd=${cmd//$'\a'/%07}
    cmd=${cmd//$'\r'/%0D}
    cmd=${cmd//$'\n'/%0A}
    __tty7_osc "133;C;$cmd"
  }

  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __tty7_precmd
  add-zsh-hook preexec __tty7_preexec
  # add-zsh-hook can only append, and the user's hooks are all registered by now
  # (their .zshrc ran before this file) — prepend the D emitter by hand so it's
  # the first thing to run when a command exits. Users who define a classic
  # `precmd()` function still get ahead of us (zsh calls it before the array);
  # that's out of reach without wrapping their function.
  precmd_functions=(__tty7_precmd_d $precmd_functions)

  # Startup kept ZDOTDIR aimed at our throwaway redirector dir so zsh read every
  # one of our startup files. Now they've all run, point it back at the user's
  # real config dir for the live session: tools that resolve state via
  # ${ZDOTDIR:-$HOME} *at runtime* (compinit's .zcompdump, lazily-compiled plugin
  # caches) — and a nested plain `zsh` — must land in the user's dir, not our
  # empty temp one. One-shot: fires on the first precmd, then removes itself.
  __tty7_restore_zdotdir() {
    ZDOTDIR=${TTY7_USER_ZDOTDIR:-$HOME}
    add-zsh-hook -d precmd __tty7_restore_zdotdir
    unfunction __tty7_restore_zdotdir
  }
  add-zsh-hook precmd __tty7_restore_zdotdir
fi
# --- end tty7 shell integration ---
"#;

const FISH_INTEGRATION: &str = r#"
# --- tty7 shell integration (fish) ---
# Guard on *emptiness* (`test -z`), not definedness (`set -q`): `setup()` resets the
# sentinel to an empty-but-exported "" at each spawn boundary, and fish reports an
# empty exported var as *set*, so `not set -q` would skip the install on every fish
# launch (OSC 133 never arms). `-z` matches the zsh/bash guards and the empty reset —
# it installs once for a fresh top-level shell while an inherited `1` still blocks it.
if status is-interactive; and test -z "$TTY7_SHELL_INTEGRATION"
  set -gx TTY7_SHELL_INTEGRATION 1

  function __tty7_osc
    printf '\e]%s\a' $argv[1]
  end

  function __tty7_report_edit_mode
    switch $fish_key_bindings
      case '*vi*'
        __tty7_osc "133;V;1"
      case '*'
        __tty7_osc "133;V;0"
    end
  end

  # The daemon percent-decodes the OSC 7 payload; escape literal `%` as %25 so
  # a path like /tmp/a%20b round-trips instead of decoding to /tmp/a b.
  function __tty7_report_cwd
    printf '\e]7;file://%s%s\a' (hostname) (string replace --all '%' '%25' -- $PWD)
  end

  # The C mark carries the submitted line, truncated and with the bytes that
  # would break OSC framing or the daemon's percent-decode escaped (% ESC BEL
  # CR NL) — the Windows agent-detection input (see core::cli_agent). fish
  # command substitution splits output on newlines, so a multi-line command
  # arrives as a list; the final `string join` re-joins it with the escaped
  # newline. `%` must be escaped first (the other escapes introduce `%`).
  function __tty7_preexec --on-event fish_preexec
    set -g __tty7_cmd_active 1
    set -l cmd (string sub -l 512 -- $argv[1] | string replace -a '%' '%25' | string replace -a \e '%1B' | string replace -a \a '%07' | string replace -a \r '%0D' | string join '%0A')
    __tty7_osc "133;C;$cmd"
  end

  # Runs on the fish_prompt *event*, which fires before fish calls the
  # fish_prompt *function* to render the prompt text — i.e. exactly where A
  # (prompt start) belongs.
  function __tty7_precmd --on-event fish_prompt
    set -l ret $status
    if set -q __tty7_cmd_active
      __tty7_osc "133;D;$ret"
      set -e __tty7_cmd_active
    end
    __tty7_report_cwd
    __tty7_report_edit_mode
    __tty7_osc "133;A"
  end

  functions -c fish_prompt __tty7_original_fish_prompt
  function fish_prompt
    __tty7_original_fish_prompt
    __tty7_osc "133;B"
  end
end
# --- end tty7 shell integration ---
"#;

const BASH_INTEGRATION: &str = r#"
# --- tty7 shell integration (bash) ---
if [[ $- == *i* ]] && [[ -z "$TTY7_SHELL_INTEGRATION" ]]; then
  export TTY7_SHELL_INTEGRATION=1

  # Per-pane history — see the zsh block for why this can only be done here,
  # after the user's rc has decided where their history lives. bash loads the
  # file after the startup files too, so repointing here still precedes the
  # load.
  if [[ -n "$TTY7_HISTFILE" ]]; then
    if [[ ! -e "$TTY7_HISTFILE" ]]; then
      # The origin is recorded even when there is nothing to copy from it yet:
      # a first-ever shell has no history file, and it is exactly that user
      # whose commands would otherwise have nowhere to go when the pane closes.
      if [[ -n "$HISTFILE" ]]; then
        builtin printf '%s\n' "$HISTFILE" > "$TTY7_HISTFILE.origin" 2>/dev/null
        if [[ -r "$HISTFILE" ]]; then
          command tail -n 5000 "$HISTFILE" > "$TTY7_HISTFILE" 2>/dev/null
        fi
      fi
      [[ -e "$TTY7_HISTFILE" ]] || : > "$TTY7_HISTFILE" 2>/dev/null
      command wc -c < "$TTY7_HISTFILE" 2>/dev/null | command tr -d ' \n' \
        > "$TTY7_HISTFILE.seed" 2>/dev/null
    fi
    HISTFILE="$TTY7_HISTFILE"
    # On exit bash rewrites the file from a memory capped at HISTSIZE, then
    # truncates it to HISTFILESIZE lines — both default to 500, which is
    # smaller than the seed. The result would be a file shorter than its own
    # seed mark, which the merge on close reads as "replaced under us" and
    # rightly refuses, losing this pane's commands. Both caps only govern the
    # pane's private file now, so raising them costs nothing. Negative means
    # unlimited and is already enough.
    if (( ${HISTSIZE:-500} >= 0 && ${HISTSIZE:-500} < 100000 )) 2>/dev/null; then
      HISTSIZE=100000
    fi
    if (( ${HISTFILESIZE:-500} >= 0 && ${HISTFILESIZE:-500} < 100000 )) 2>/dev/null; then
      HISTFILESIZE=100000
    fi
  fi

  __tty7_osc() { builtin printf '\e]%s\a' "$1"; }
  # `bind -v` reports readline's actual editing mode; `[[ -o vi ]]` misses
  # vi mode configured only in ~/.inputrc (`set editing-mode vi` flips
  # readline without setting the shell option).
  __tty7_report_edit_mode() {
    if [[ "$(builtin bind -v 2>/dev/null)" == *"set editing-mode vi"* ]]; then
      __tty7_osc "133;V;1"
    else
      __tty7_osc "133;V;0"
    fi
  }
  # Escape literal `%` as %25 — the daemon percent-decodes the OSC 7 payload.
  #
  # Under Git Bash (msys) `$PWD` is an msys path — `/c/Users/x`, and `/tmp` for
  # mounts with no drive at all. The daemon runs Windows-side, where `/c/Users/x`
  # is not absolute but *drive-relative*, so it resolves to a bogus `C:\c\Users\x`
  # (see `pane::strip_uri_drive_slash`, which only un-prefixes `/C:/…`). `pwd -W`
  # is msys's own translation to the real Windows path, and it resolves mounts
  # that have a real backing directory (`/tmp` -> `C:/Users/x/AppData/Local/Temp`).
  # It has no leading slash, so add one to make it the absolute-path shape a
  # file: URI expects.
  #
  # For msys-only virtual mounts (`/proc`, `/dev`) there *is* no Windows path
  # and `pwd -W` is the identity, so require a drive letter and stay silent
  # otherwise — a `/proc` payload would land as drive-relative `C:\proc` and
  # fail the next spawn, whereas reporting nothing leaves the daemon holding
  # the last usable cwd. Testing the shape beats testing for a leading slash,
  # which cannot tell a translated path from an untranslated one.
  #
  # The branch is resolved once at install time; the `$(…)` inside still forks
  # per prompt (~6.8 ms under msys, vs ~0.3 ms for the plain `$PWD` path), which
  # is the price of a correct path and only paid by Git Bash panes.
  if [[ "$OSTYPE" == msys* || "$OSTYPE" == cygwin* ]]; then
    __tty7_report_cwd() {
      local d
      d="$(builtin pwd -W 2>/dev/null)" || return 0
      [[ "$d" == ?:* ]] || return 0
      builtin printf '\e]7;file://%s/%s\a' "${HOSTNAME:-localhost}" "${d//\%/%25}"
    }
  else
    __tty7_report_cwd() { builtin printf '\e]7;file://%s%s\a' "${HOSTNAME:-localhost}" "${PWD//\%/%25}"; }
  fi

  # Own hook for D, prepended to precmd_functions (same rationale as the zsh
  # path): the app flips back to prompt-editing on D, so it must fire the
  # instant the command exits, not after the user's precmd functions.
  __tty7_precmd_d() {
    local ret=$?
    if [[ -n "$__tty7_cmd_active" ]]; then
      __tty7_osc "133;D;$ret"
      unset __tty7_cmd_active
    fi
    return $ret
  }

  __tty7_precmd() {
    local ret=$?
    __tty7_report_cwd
    __tty7_report_edit_mode
    __tty7_osc "133;A"
    # Prompt-end marker (B), wrapped in \[...\] so readline excludes it from the
    # prompt's on-screen width. Re-appended every precmd (like the zsh path)
    # since prompt frameworks that rebuild PS1 in their own precmd would
    # otherwise drop it; the case-check keeps a static PS1 from accumulating
    # duplicates.
    case "$PS1" in
      *'\[\033]133;B\a\]'*) ;;
      *) PS1="$PS1"'\[\033]133;B\a\]' ;;
    esac
    return $ret
  }

  # The C mark carries the submitted line ($1, from bash-preexec), truncated
  # and escaped the same way as the zsh path — the Windows agent-detection
  # input (git-bash; see core::cli_agent).
  __tty7_preexec() {
    __tty7_cmd_active=1
    local cmd=${1:0:512}
    cmd=${cmd//\%/%25}
    cmd=${cmd//$'\e'/%1B}
    cmd=${cmd//$'\a'/%07}
    cmd=${cmd//$'\r'/%0D}
    cmd=${cmd//$'\n'/%0A}
    __tty7_osc "133;C;$cmd"
  }

  if [[ -z "${bash_preexec_imported:-}" ]]; then
    # --- vendored from bash-preexec.sh (https://github.com/rcaloras/bash-preexec, MIT) ---
    bash_preexec_imported="defined"
    __bp_imported="$bash_preexec_imported"

    __bp_last_ret_value="$?"
    BP_PIPESTATUS=("${PIPESTATUS[@]}")
    __bp_last_argument_prev_command="$_"
    __bp_inside_precmd=0
    __bp_inside_preexec=0
    __bp_preexec_interactive_mode=""
    __bp_install_string=$'__bp_trap_string="$(trap -p DEBUG)"\ntrap - DEBUG\n__bp_install'

    declare -a precmd_functions
    declare -a preexec_functions

    __bp_require_not_readonly() {
      local var
      for var; do
        if ! ( unset "$var" 2> /dev/null ); then
          echo "bash-preexec requires write access to ${var}" >&2
          return 1
        fi
      done
    }

    __bp_trim_whitespace() {
      local var=${1:?} text=${2:-}
      text="${text#"${text%%[![:space:]]*}"}"
      text="${text%"${text##*[![:space:]]}"}"
      printf -v "$var" '%s' "$text"
    }

    __bp_sanitize_string() {
      local var=${1:?} text=${2:-} sanitized
      __bp_trim_whitespace sanitized "$text"
      sanitized=${sanitized%;}
      sanitized=${sanitized#;}
      __bp_trim_whitespace sanitized "$sanitized"
      printf -v "$var" '%s' "$sanitized"
    }

    __bp_interactive_mode() { __bp_preexec_interactive_mode="on"; }

    __bp_precmd_invoke_cmd() {
      __bp_last_ret_value="$?" BP_PIPESTATUS=("${PIPESTATUS[@]}")
      if (( __bp_inside_precmd > 0 )); then return; fi
      local __bp_inside_precmd=1
      local precmd_function
      for precmd_function in "${precmd_functions[@]}"; do
        if type -t "$precmd_function" 1>/dev/null; then
          __bp_set_ret_value "$__bp_last_ret_value" "$__bp_last_argument_prev_command"
          "$precmd_function"
        fi
      done
      __bp_set_ret_value "$__bp_last_ret_value"
    }

    __bp_set_ret_value() { return ${1:+"$1"}; }

    __bp_in_prompt_command() {
      local prompt_command_array IFS=$'\n;'
      read -rd '' -a prompt_command_array <<< "${PROMPT_COMMAND[*]:-}"
      local trimmed_arg
      __bp_trim_whitespace trimmed_arg "${1:-}"
      local command trimmed_command
      for command in "${prompt_command_array[@]:-}"; do
        __bp_trim_whitespace trimmed_command "$command"
        if [[ "$trimmed_command" == "$trimmed_arg" ]]; then return 0; fi
      done
      return 1
    }

    __bp_preexec_invoke_exec() {
      __bp_last_argument_prev_command="${1:-}"
      if (( __bp_inside_preexec > 0 )); then return; fi
      local __bp_inside_preexec=1
      if [[ ! -t 1 && -z "${__bp_delay_install:-}" ]]; then return; fi
      if [[ -n "${COMP_LINE:-}" ]]; then return; fi
      if [[ -n "${READLINE_LINE+x}" ]]; then return; fi
      if [[ -z "${__bp_preexec_interactive_mode:-}" ]]; then
        return
      else
        if [[ 0 -eq "${BASH_SUBSHELL:-}" ]]; then
          __bp_preexec_interactive_mode=""
        fi
      fi
      if __bp_in_prompt_command "${BASH_COMMAND:-}"; then
        __bp_preexec_interactive_mode=""
        return
      fi
      local this_command
      this_command=$(
        export LC_ALL=C
        HISTTIMEFORMAT='' builtin history 1 | sed '1 s/^ *[0-9][0-9]*[* ] //'
      )
      if [[ -z "$this_command" ]]; then return; fi
      local preexec_function
      local preexec_function_ret_value
      local preexec_ret_value=0
      for preexec_function in "${preexec_functions[@]:-}"; do
        if type -t "$preexec_function" 1>/dev/null; then
          __bp_set_ret_value "${__bp_last_ret_value:-}"
          "$preexec_function" "$this_command"
          preexec_function_ret_value="$?"
          if [[ "$preexec_function_ret_value" != 0 ]]; then
            preexec_ret_value="$preexec_function_ret_value"
          fi
        fi
      done
      __bp_set_ret_value "$preexec_ret_value" "$__bp_last_argument_prev_command"
    }

    __bp_install() {
      if [[ "${PROMPT_COMMAND[*]:-}" == *"__bp_precmd_invoke_cmd"* ]]; then return 1; fi
      trap '__bp_preexec_invoke_exec "$_"' DEBUG
      local prior_trap
      prior_trap=$(sed "s/[^']*'\(.*\)'[^']*/\1/" <<<"${__bp_trap_string:-}")
      unset __bp_trap_string
      if [[ -n "$prior_trap" ]]; then
        eval '__bp_original_debug_trap() {
          '"$prior_trap"'
        }'
        preexec_functions+=(__bp_original_debug_trap)
      fi
      if [[ -n "${__bp_enable_subshells:-}" ]]; then
        set -o functrace > /dev/null 2>&1
        shopt -s extdebug > /dev/null 2>&1
      fi;
      local existing_prompt_command
      existing_prompt_command="${PROMPT_COMMAND:-}"
      existing_prompt_command="${existing_prompt_command//$__bp_install_string/:}"
      existing_prompt_command="${existing_prompt_command//$'\n':$'\n'/$'\n'}"
      existing_prompt_command="${existing_prompt_command//$'\n':;/$'\n'}"
      __bp_sanitize_string existing_prompt_command "$existing_prompt_command"
      if [[ "${existing_prompt_command:-:}" == ":" ]]; then
        existing_prompt_command=
      fi
      PROMPT_COMMAND='__bp_precmd_invoke_cmd'
      PROMPT_COMMAND+=${existing_prompt_command:+$'\n'$existing_prompt_command}
      if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
        PROMPT_COMMAND+=('__bp_interactive_mode')
      else
        PROMPT_COMMAND+=$'\n__bp_interactive_mode'
      fi
      precmd_functions+=(precmd)
      preexec_functions+=(preexec)
      __bp_precmd_invoke_cmd
      __bp_interactive_mode
    }

    __bp_install_after_session_init() {
      __bp_require_not_readonly PROMPT_COMMAND HISTCONTROL HISTTIMEFORMAT || return
      local sanitized_prompt_command
      __bp_sanitize_string sanitized_prompt_command "${PROMPT_COMMAND:-}"
      if [[ -n "$sanitized_prompt_command" ]]; then
        PROMPT_COMMAND=${sanitized_prompt_command}$'\n'
      fi;
      PROMPT_COMMAND+=${__bp_install_string}
    }
    # --- end vendored bash-preexec.sh ---

    __bp_install_after_session_init
  fi

  # D first (before any user precmds bash-preexec already knows about), the
  # prompt bookkeeping last — mirroring the zsh registration order.
  precmd_functions=(__tty7_precmd_d "${precmd_functions[@]}")
  precmd_functions+=(__tty7_precmd)
  preexec_functions+=(__tty7_preexec)
fi
# --- end tty7 shell integration ---
"#;

const POWERSHELL_INTEGRATION: &str = r#"
# --- tty7 shell integration (PowerShell) ---
if (-not $env:TTY7_SHELL_INTEGRATION) {
  $env:TTY7_SHELL_INTEGRATION = '1'

  $global:__Tty7Esc = [char]0x1b
  $global:__Tty7Bel = [char]0x07
  # Whatever prompt the user's profile settled on; we call through to it.
  $global:__Tty7OrigPrompt = $function:prompt
  # Gates the D marker so the first prompt (no command yet) emits no bogus exit.
  $global:__Tty7CmdActive = $false

  # Who and where, for the OSC 7 host and the OSC 0 title. Read through .NET
  # rather than the USERNAME / COMPUTERNAME / USERPROFILE environment variables:
  # those spellings exist only on Windows, and on macOS and Linux all three come
  # back empty, which left every pwsh pane there titled `@:` followed by a full
  # un-abbreviated path (issue #583). $HOME is PowerShell's own automatic
  # variable and is right on every platform. Resolved once — none of the three
  # changes within a session.
  $global:__Tty7User = [Environment]::UserName
  $global:__Tty7Host = [Environment]::MachineName
  $global:__Tty7Home = if ($HOME) { $HOME.Replace('\', '/').TrimEnd('/') } else { '' }

  function global:prompt {
    # $? first: an assignment sets $? to true, so read it before anything else.
    $ok = $?
    $lastExit = $LASTEXITCODE

    if ($global:__Tty7CmdActive) {
      $global:__Tty7CmdActive = $false
      # $? is the reliable success signal; $LASTEXITCODE can be stale, so only
      # trust it when $? already says the command failed.
      $code = if ($ok) { 0 } elseif ($lastExit) { $lastExit } else { 1 }
      Write-Host -NoNewline "$($global:__Tty7Esc)]133;D;$code$($global:__Tty7Bel)"
    }

    # cwd + title, for real filesystem locations only.
    if ($PWD.Provider.Name -eq 'FileSystem') {
      $fsPath = $PWD.ProviderPath

      # OSC 7 cwd. Escape a literal % as %25 (the daemon percent-decodes the
      # payload) and use forward slashes. Force one leading slash so a Windows
      # drive path (`C:/…`) becomes `/C:/…` — the absolute-path shape the URI
      # expects — while a POSIX path keeps its single slash instead of doubling it.
      $p = $fsPath.Replace('%', '%25').Replace('\', '/')
      if (-not $p.StartsWith('/')) { $p = '/' + $p }
      Write-Host -NoNewline "$($global:__Tty7Esc)]7;file://$($global:__Tty7Host)$p$($global:__Tty7Bel)"

      # OSC 0 window/tab title "user@host:dir". Neither a PowerShell profile nor
      # pwsh itself sets a useful title — on macOS pwsh emits an *empty* OSC 0 —
      # so without this every tty7 tab running PowerShell stays generic, on every
      # platform. Forward slashes (so tty7's tab-label parser can take the last
      # path segment) and home shown as `~`. Re-emitted each prompt so it tracks
      # cwd; a full-screen app's own title still overrides it while it runs.
      #
      # The home match needs the separator, not just the prefix: with a home of
      # `/Users/ann`, a bare StartsWith also swallows `/Users/annex`, retitling it
      # `~ex`.
      $titlePath = $fsPath.Replace('\', '/')
      if ($global:__Tty7Home -and ($titlePath -eq $global:__Tty7Home -or
          $titlePath.StartsWith($global:__Tty7Home + '/'))) {
        $titlePath = '~' + $titlePath.Substring($global:__Tty7Home.Length)
      }
      Write-Host -NoNewline "$($global:__Tty7Esc)]0;$($global:__Tty7User)@$($global:__Tty7Host):$titlePath$($global:__Tty7Bel)"
    }

    # Restore the captured status so the user's own prompt sees the real result,
    # then re-restore $LASTEXITCODE afterwards in case the prompt clobbered it.
    $global:LASTEXITCODE = $lastExit
    if (-not $ok) { Write-Error '' -ErrorAction Ignore }
    $base = & $global:__Tty7OrigPrompt
    if ($base -is [array]) { $base = $base -join [char]0x0a }
    $global:LASTEXITCODE = $lastExit

    "$($global:__Tty7Esc)]133;A$($global:__Tty7Bel)$base$($global:__Tty7Esc)]133;B$($global:__Tty7Bel)"
  }

  # C (command output begins): wrap PSReadLine's line reader. Force-load the
  # module first so the function exists even if the host hasn't imported it yet;
  # all best-effort (an empty Enter submits no command, so it arms nothing).
  Import-Module PSReadLine -ErrorAction SilentlyContinue
  if ((Test-Path Function:\PSConsoleHostReadLine) -and -not $global:__Tty7ReadLineWrapped) {
    $global:__Tty7ReadLineWrapped = $true
    $global:__Tty7OrigReadLine = $function:global:PSConsoleHostReadLine
    function global:PSConsoleHostReadLine {
      $line = & $global:__Tty7OrigReadLine
      if (-not [string]::IsNullOrWhiteSpace($line)) {
        $global:__Tty7CmdActive = $true
        # Carry the submitted line on the C mark (tty7 extension): the daemon
        # detects coding agents from it on Windows, where ConPTY exposes no
        # foreground process group to read an argv from. Truncated (detection
        # only reads the front) and percent-encoded so the payload can't carry
        # a raw `;`, ESC or BEL into the OSC framing. The cut can split a
        # surrogate pair, and a lone high surrogate makes EscapeDataString
        # throw on .NET Framework (PS 5.1) — inside this wrapper that would
        # swallow the submitted line, so drop it and keep the whole mark
        # best-effort: a plain `C` still flips the prompt state.
        $cmd = if ($line.Length -gt 512) { $line.Substring(0, 512) } else { $line }
        if ([char]::IsHighSurrogate($cmd[$cmd.Length - 1])) {
          $cmd = $cmd.Substring(0, $cmd.Length - 1)
        }
        try {
          $cmd = [Uri]::EscapeDataString($cmd)
          Write-Host -NoNewline "$($global:__Tty7Esc)]133;C;$cmd$($global:__Tty7Bel)"
        } catch {
          Write-Host -NoNewline "$($global:__Tty7Esc)]133;C$($global:__Tty7Bel)"
        }
      }
      $line
    }
  }
}
# --- end tty7 shell integration ---
"#;

const NUSHELL_INTEGRATION: &str = r#"
# --- tty7 shell integration (nushell) ---
# Guard on emptiness (`== ""`), not mere definedness, exactly like the other
# shells: setup() resets the sentinel to an empty-but-exported value at each
# spawn boundary, and a nested `nu` inherits the set `1` without re-installing.
if (($env.TTY7_SHELL_INTEGRATION? | default "") == "") {
  $env.TTY7_SHELL_INTEGRATION = "1"

  # tty7 restores the user's own config.nu at this placeholder — `source` is
  # evaluated at parse time, so the wrapper can only name a file that provably
  # exists; the Rust side resolves the path (the same resolution
  # `$nu.default-config-dir` performs) and substitutes a literal, or a no-op
  # line when there is no config.nu. The order is load-bearing: a config.nu
  # conventionally ends with a wholesale `$env.config = {...}` that would
  # clobber every hook added below, so the hooks must come after it.
  __TTY7_SOURCE_USER_CONFIG__

  # Hooks live in `$env.config.hooks` as lists. Ensure the skeleton exists
  # before appending to it — a minimal config, or none at all, leaves it
  # missing, and a bare assignment would error instead of creating it.
  if (($env.config.hooks? | default {}) | is-empty) {
    $env.config = ($env.config | upsert hooks { pre_prompt: [], pre_execution: [], env_change: {} })
  }

  $env.config.hooks.pre_prompt = ($env.config.hooks.pre_prompt | default [] | append {||
    # OSC 133 prompt-start (A) and the previous command's exit (D). D is gated
    # on a flag the pre_execution hook arms, so the very first prompt emits no
    # bogus exit status — the same rule every other integration follows.
    let __tty7_osc = {|__tty7_payload| print -n $"(ansi osc)($__tty7_payload)(char bel)" }
    if ($env.__tty7_cmd_active? | default false) {
      hide-env __tty7_cmd_active
      do $__tty7_osc $"133;D;($env.LAST_EXIT_CODE? | default 0)"
    }
    # OSC 7 cwd, so the daemon tracks the pane's directory across `cd`. The
    # payload is percent-decoded on the daemon side, so a literal `%` must be
    # escaped as %25, and a Windows drive path needs the leading slash that
    # makes `C:/…` an absolute URI path (`/C:/…`). Backslashes are separators
    # only on Windows — a Unix path may legally contain one, so that
    # translation is gated on the platform the way nu's own OSC 7 gates it.
    let __tty7_path = if (($nu.os-info.name? | default '') == 'windows') {
      ($env.PWD | str replace -a '\' '/' | str replace -a '%' '%25')
    } else {
      ($env.PWD | str replace -a '%' '%25')
    }
    let __tty7_path = if ($__tty7_path | str starts-with '/') { $__tty7_path } else { '/' + $__tty7_path }
    do $__tty7_osc $"7;file://($env.COMPUTERNAME? | default 'localhost')($__tty7_path)"
    do $__tty7_osc "133;A"
  })

  # OSC 133 command-output-begins (C) plus the flag that gates the D report.
  # Nushell's pre_execution hook receives no command text, so C carries no
  # payload — the daemon still flips to "command running" on the bare mark.
  $env.config.hooks.pre_execution = ($env.config.hooks.pre_execution | default [] | append {||
    $env.__tty7_cmd_active = true
    print -n $"(ansi osc)133;C(char bel)"
  })

  # OSC 133 prompt-end (B) must land after the very last prompt character, and
  # the only hook-shaped place Nushell runs at that point is prompt_indicator —
  # so wrap it, but only when the config defines one. A missing indicator is
  # the built-in default prompt's to render (recent Nushells draw it and their
  # own B mark themselves), and replacing it here would erase the glyph.
  if ((($env.config.prompt_indicator? | default null) | describe) != 'nothing') {
    let __tty7_orig_indicator = $env.config.prompt_indicator
    $env.config.prompt_indicator = {||
      let __tty7_ind = match ($__tty7_orig_indicator | describe) {
        'closure' => (do $__tty7_orig_indicator)
        _ => $__tty7_orig_indicator
      }
      $"($__tty7_ind)(ansi osc)133;B(char bel)"
    }
  }
}
# --- end tty7 shell integration ---
"#;

fn zsh_redirectors() -> [(&'static str, String); 4] {
    let redirect = |name: &str, tail: &str| {
        format!(
            "__tty7_ztmp=$ZDOTDIR\n\
             if [[ -n \"$TTY7_USER_ZDOTDIR\" ]]; then ZDOTDIR=$TTY7_USER_ZDOTDIR; else unset ZDOTDIR; fi\n\
             [[ -f \"${{ZDOTDIR:-$HOME}}/{name}\" ]] && source \"${{ZDOTDIR:-$HOME}}/{name}\"\n\
             {tail}ZDOTDIR=$__tty7_ztmp\n\
             unset __tty7_ztmp\n"
        )
    };
    [
        (
            ".zshenv",
            redirect(".zshenv", "export TTY7_USER_ZDOTDIR=${ZDOTDIR:-$HOME}\n"),
        ),
        (".zprofile", redirect(".zprofile", "")),
        (
            ".zshrc",
            format!("{}{ZSH_INTEGRATION}", redirect(".zshrc", "")),
        ),
        (".zlogin", redirect(".zlogin", "")),
    ]
}

pub(crate) struct Injection {
    pub env: HashMap<String, String>,
    pub args: Vec<String>,
    pub replaces_argv: bool,
    pub dir: Option<PathBuf>,
}

const ZDOTDIR_PREFIX: &str = "tty7-zdotdir-";
const NU_PREFIX: &str = "tty7-nu-";
const BASHRC_PREFIX: &str = "tty7-bashrc-";
const WSLRC_PREFIX: &str = "tty7-wslrc-";

/// Every prefix [`throwaway_dir`] hands out for a shell's scratch directory.
///
/// The sweep reads this list rather than one prefix it happened to know about,
/// which is the difference between clearing zsh's leavings and clearing all of
/// them: bash's had piled up to 607 on this machine while zsh's were being
/// swept. A shell added to the `setup_*` functions below is covered the moment
/// it takes its directory from one of these.
const THROWAWAY_PREFIXES: [&str; 4] = [ZDOTDIR_PREFIX, NU_PREFIX, BASHRC_PREFIX, WSLRC_PREFIX];

fn is_our_zdotdir(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(ZDOTDIR_PREFIX))
}

fn real_user_zdotdir() -> Option<String> {
    if let Ok(z) = std::env::var("TTY7_USER_ZDOTDIR")
        && !z.is_empty()
    {
        return Some(z);
    }
    std::env::var("ZDOTDIR")
        .ok()
        .filter(|z| !z.is_empty() && !is_our_zdotdir(z))
}

enum ShellKind {
    Zsh,
    Bash,
    Fish,
    PowerShell,
    Nushell,
    Wsl,
}

fn shell_kind(program: Option<&str>) -> Option<ShellKind> {
    let owned = match program {
        Some(p) => p.to_string(),
        None => crate::core::shells::login_shell(),
    };
    let base = Path::new(&owned)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    match base.strip_suffix(".exe").unwrap_or(&base) {
        "zsh" => Some(ShellKind::Zsh),
        "bash" if cfg!(windows) && !is_msys_bash(&owned) => None,
        "bash" => Some(ShellKind::Bash),
        "fish" => Some(ShellKind::Fish),
        "powershell" | "pwsh" => Some(ShellKind::PowerShell),
        "nu" => Some(ShellKind::Nushell),
        "wsl" => Some(ShellKind::Wsl),
        _ => None,
    }
}

pub(crate) fn wsl_distro(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--distribution" || a == "-d" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--distribution=") {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg_attr(not(windows), allow(dead_code))]
fn wslenv_with(existing: Option<&str>, additions: &[&str]) -> String {
    let mut out: Vec<String> = existing
        .unwrap_or("")
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    for add in additions {
        let name = add.split('/').next().unwrap_or(add);
        if !out.iter().any(|e| e.split('/').next().unwrap_or(e) == name) {
            out.push((*add).to_string());
        }
    }
    out.join(":")
}

fn is_msys_bash(program: &str) -> bool {
    if !cfg!(windows) {
        return true;
    }
    let normalized = program.replace('/', "\\").to_ascii_lowercase();
    let Some((dir, _)) = normalized.rsplit_once('\\') else {
        return false;
    };
    let system_root = std::env::var("SystemRoot")
        .unwrap_or_else(|_| r"C:\Windows".to_string())
        .replace('/', "\\")
        .to_ascii_lowercase();
    let system_root = system_root.trim_end_matches('\\');
    !(dir == system_root || dir.starts_with(&format!("{system_root}\\")))
}

/// A scratch directory for a shell's startup files, owned by this user alone.
///
/// What goes in here is *sourced by the shell*: `setup_zsh` points `ZDOTDIR`
/// at it, and zsh then reads `.zshenv`, `.zprofile`, `.zshrc` and `.zlogin`
/// out of it on every pane. bash and nu are pointed at theirs the same way.
///
/// The name is `<prefix><pid>-<seq>`, which the reaper depends on and which is
/// therefore entirely predictable — and on Linux the system temp directory is
/// `/tmp`, mode 1777, where any account can create a name before this does.
/// `create_dir_all` succeeds on a directory that is already there, so the
/// files would have been written into one somebody else owned, who could then
/// rewrite them in the moment between the write and the shell reading them.
///
/// So: created, never adopted. `DirBuilder::create` fails if the path exists,
/// and the mode rides on the creating syscall rather than a chmod afterwards,
/// so there is no instant at which the directory is present and readable by
/// anyone else. A name already taken is not an error to report but a name to
/// step over — that is all an attacker holding one can accomplish.
fn throwaway_dir(prefix: &str) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    for _ in 0..ATTEMPTS {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("{prefix}{}-{seq}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => return Some(dir),
            // Ours from an earlier pane in this process, or somebody else's.
            // Either way the next sequence number is untaken.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                log::warn!(
                    "could not make a shell scratch dir at {}: {e}",
                    dir.display()
                );
                return None;
            }
        }
    }
    log::warn!(
        "gave up making a shell scratch directory under {}",
        base.display()
    );
    None
}

/// How many names [`throwaway_dir`] will try before giving up. Each failure
/// means the name was taken, and the shell simply starts without tty7's
/// integration rather than reading files from a directory it does not own.
const ATTEMPTS: usize = 64;

/// The `<pid>` a throwaway directory's name carries, if it carries one.
///
/// [`throwaway_dir`] names them `<prefix><pid>-<seq>`, so the owner is the text
/// between a known prefix and the first `-` after it. Anything else — the
/// `tty7-zdotdir-wsl` a WSL route writes inside its own directory, a name
/// someone else happened to choose — answers `None` and is left alone.
fn throwaway_owner(name: &str) -> Option<u32> {
    THROWAWAY_PREFIXES
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix)?.split_once('-')?.0.parse().ok())
}

/// Remove the throwaway shell directories left by daemons that are gone.
///
/// A pane's teardown removes its own, so a daemon that stops cleanly leaves
/// none. One that is killed never runs that teardown, and its directories then
/// stay in the temp dir for good — and nothing ever looks at them again. This
/// machine had 3,850 of zsh's and 607 of bash's from months of crashes and
/// `kill -9`s, which is the same shape as the socket a killed daemon used to
/// leave: litter only a later startup is in a position to notice.
///
/// Deliberately timid. It removes a directory only when the name is exactly
/// ours, the pid parses, and that pid is not a live process — so a running
/// daemon's directories are never touched, and a pid that has since been
/// reused just means the directory waits for another day. On a platform with
/// no cheap liveness query [`daemon_process_alive`](crate::daemon::spawn::daemon_process_alive)
/// answers "alive", and this does nothing at all.
pub(crate) fn sweep_dead_zdotdirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let mine = std::process::id();
    let mut swept = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(owner) = throwaway_owner(name) else {
            continue;
        };
        if owner == mine || crate::daemon::spawn::daemon_process_alive(owner) {
            continue;
        }
        if entry.path().is_dir() && std::fs::remove_dir_all(entry.path()).is_ok() {
            swept += 1;
        }
    }
    if swept > 0 {
        log::info!("swept {swept} shell-integration directories left by daemons that are gone");
    }
}

fn setup_zsh() -> Option<Injection> {
    let dir = throwaway_dir(ZDOTDIR_PREFIX)?;
    for (name, contents) in zsh_redirectors() {
        std::fs::write(dir.join(name), contents).ok()?;
    }

    let mut env = HashMap::new();
    if let Some(user_zdotdir) = real_user_zdotdir() {
        env.insert("TTY7_USER_ZDOTDIR".to_string(), user_zdotdir);
    }
    env.insert("ZDOTDIR".to_string(), dir.to_string_lossy().into_owned());

    Some(Injection {
        env,
        args: Vec::new(),
        replaces_argv: false,
        dir: Some(dir),
    })
}

fn setup_fish() -> Option<Injection> {
    Some(Injection {
        env: HashMap::new(),
        args: vec!["-C".to_string(), FISH_INTEGRATION.to_string()],
        replaces_argv: false,
        dir: None,
    })
}

fn setup_powershell() -> Option<Injection> {
    Some(Injection {
        env: HashMap::new(),
        args: vec![
            "-NoLogo".to_string(),
            "-NoExit".to_string(),
            "-EncodedCommand".to_string(),
            powershell_encoded_command(POWERSHELL_INTEGRATION),
        ],
        replaces_argv: false,
        dir: None,
    })
}

/// Nushell has no environment variable that redirects its config file the way
/// ZDOTDIR does for zsh, so the injection rides `--config` instead: a
/// throwaway config.nu that sources the user's real one and then appends the
/// OSC hooks. The `dir` is what makes the file disappear when the pane closes.
///
/// Trade-off, inherent to `--config`: `$nu.config-path` points at the wrapper,
/// so `config nu` inside the pane edits a file that vanishes with it. The zsh
/// path dodges the analogue by restoring ZDOTDIR after startup; Nushell's
/// `$nu.*` paths are immutable, so this is accepted rather than fought.
fn setup_nushell() -> Option<Injection> {
    setup_nushell_with(nushell_user_config_path().as_deref())
}

/// Split from `setup_nushell` so a test can drive the whole wrapper against a
/// config.nu of its own without writing into the real one.
fn setup_nushell_with(user_config: Option<&Path>) -> Option<Injection> {
    let dir = throwaway_dir(NU_PREFIX)?;
    let config = dir.join("config.nu");
    std::fs::write(&config, nushell_config_script_with(user_config)).ok()?;

    Some(Injection {
        env: HashMap::new(),
        args: vec![
            "--config".to_string(),
            config.to_string_lossy().into_owned(),
        ],
        replaces_argv: false,
        dir: Some(dir),
    })
}

/// The wrapper config.nu: the user's real config.nu sourced back in when it
/// exists, then the OSC hooks. `source` is a parse-time construct in Nushell —
/// it cannot be guarded at runtime or name a missing file — so the path is
/// resolved and substituted here, and a machine without a config.nu gets a
/// no-op line instead.
fn nushell_config_script_with(user_config: Option<&Path>) -> String {
    let source = match user_config {
        Some(path) => format!("source {}", nu_string_literal(&path.to_string_lossy())),
        None => "# no user config.nu to restore".to_string(),
    };
    NUSHELL_INTEGRATION.replace("__TTY7_SOURCE_USER_CONFIG__", &source)
}

/// Where nu would load its config.nu from. Only a file that exists comes
/// back — the wrapper's `source` cannot name one that is not there.
fn nushell_user_config_path() -> Option<PathBuf> {
    let config = nushell_config_dir()?.join("config.nu");
    config.is_file().then_some(config)
}

/// nu's own config directory, resolved the way nu resolves it. Getting this
/// wrong is silent and total: the wrapper reports "no config.nu to restore"
/// for a user who has one, and `nu --config` then replaces their config with
/// one that never sources it.
///
/// nu-path's `configurable_dir_path` consults `$XDG_CONFIG_HOME` on *every*
/// platform, Windows included, and only when it is non-empty **and** absolute;
/// anything else falls through to `dirs::config_dir()`.
fn nushell_config_dir() -> Option<PathBuf> {
    nushell_config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        platform_config_dir(),
    )
}

fn nushell_config_dir_from(
    xdg_config_home: Option<&std::ffi::OsStr>,
    platform_default: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = match xdg_config_home {
        Some(xdg) if !xdg.is_empty() && Path::new(xdg).is_absolute() => PathBuf::from(xdg),
        _ => platform_default?,
    };
    Some(base.join("nushell"))
}

/// `dirs::config_dir()` — what nu falls back to. Note this is **not** where
/// tty7 keeps its own config (`~/.config/tty7` on macOS too); nu follows the
/// platform convention, and the two are only the same directory on Linux.
#[cfg(windows)]
fn platform_config_dir() -> Option<PathBuf> {
    // `dirs` asks Windows for FOLDERID_RoamingAppData; `%APPDATA%` is the
    // same directory in every environment tty7 can spawn a pane in.
    std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn platform_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join("Library/Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".config"))
}

/// A Nushell string literal for `path`. Single quotes are fully literal in
/// Nushell, so they are the default; a path that contains an apostrophe (legal
/// on Windows) falls back to double quotes with backslash escaping.
fn nu_string_literal(path: &str) -> String {
    if !path.contains('\'') {
        return format!("'{path}'");
    }
    let escaped = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$");
    format!("\"{escaped}\"")
}

fn powershell_encoded_command(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64_encode(&utf16le)
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn bash_rcfile() -> String {
    format!(
        r#"
# Replays what a real *login* shell would have sourced, in the same order —
# necessary because tty7 spawns bash non-login (see shell_integration.rs) so
# that --rcfile is honored at all; bash silently ignores it for login shells.
#
# ~/.bashrc needs both halves of a rule that cannot be written as a static
# order, because the two shapes in the wild want opposite things:
#
#   * a ~/.bash_profile that forwards to ~/.bashrc (the common case, and what
#     every "put this in your .bash_profile" guide produces). Sourcing ~/.bashrc
#     again afterwards runs the user's whole file twice: banners print twice,
#     completions get sourced twice, anything appending to PROMPT_COMMAND stacks.
#   * a ~/.bash_profile that does *not* forward — written by someone whose
#     terminal spawns non-login shells, so ~/.bashrc always arrived on its own
#     and the profile only ever had to hold login-time settings. Dropping
#     ~/.bashrc from the chain would take that user's aliases, prompt and
#     completions away entirely.
#
# So watch instead of guess: wrap `source`/`.` for the length of the chain, note
# whether ~/.bashrc came through, and fill it in afterwards only if it did not.
# `~` is already expanded to $HOME by the time the wrapper sees $1, so matching
# on the trailing path component covers `. ~/.bashrc`, `source "$HOME/.bashrc"`
# and a spelled-out absolute path alike.
__tty7_bashrc=0
source() {{ case "${{1-}}" in */.bashrc|.bashrc) __tty7_bashrc=1;; esac; builtin source "$@"; }}
.() {{ case "${{1-}}" in */.bashrc|.bashrc) __tty7_bashrc=1;; esac; builtin . "$@"; }}
if [[ -f /etc/profile ]]; then source /etc/profile; fi
if [[ -f ~/.bash_profile ]]; then
  source ~/.bash_profile
elif [[ -f ~/.bash_login ]]; then
  source ~/.bash_login
elif [[ -f ~/.profile ]]; then
  source ~/.profile
fi
unset -f source .
if [[ $__tty7_bashrc == 0 && -f ~/.bashrc ]]; then source ~/.bashrc; fi
unset __tty7_bashrc
{BASH_INTEGRATION}"#
    )
}

fn bash_path(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s
    }
}

fn setup_bash() -> Option<Injection> {
    let dir = throwaway_dir(BASHRC_PREFIX)?;
    let rcfile = dir.join("bashrc");
    std::fs::write(&rcfile, bash_rcfile()).ok()?;

    Some(Injection {
        env: HashMap::new(),
        args: vec!["--rcfile".to_string(), bash_path(&rcfile), "-i".to_string()],
        replaces_argv: true,
        dir: Some(dir),
    })
}

const WSL_RCFILE_ENV: &str = "TTY7_RC";

/// Where the zsh redirectors landed, in the Windows spelling `WSLENV`'s `/p`
/// translates for the distro.
///
/// Deliberately not `ZDOTDIR`: everything in this map is handed to *every* WSL
/// pane, and a bash or fish distro that found a `ZDOTDIR` in its environment
/// would carry it into any `zsh` started inside it. The bootstrap decides
/// whether this becomes `ZDOTDIR`, and it only decides that for a zsh distro.
const WSL_ZDOTDIR_ENV: &str = "TTY7_ZDOTDIR";

use crate::core::shells::shell_quote;

/// The bootstrap `sh` runs inside the distro. `$SHELL` is the only place the
/// user's real shell is named, so every arm dispatches on it: bash re-execs
/// through the rcfile written on the Windows side, zsh points `ZDOTDIR` at the
/// redirectors written beside it, fish carries its integration inline the way
/// `remote::bootstrap_command` does over SSH, and anything else falls back to a
/// plain login shell.
///
/// Both file-backed arms ask whether what they were handed is really there
/// before trusting it. The files live on the Windows side and reach the distro
/// over `/mnt`, which is not a given: automount can be off, `/etc/wsl.conf` can
/// move the root, and a distro can have no drvfs at all. Unguarded, bash is
/// handed a `--rcfile` it cannot read — which it ignores in silence, taking the
/// user's own `.bashrc` down with it, because tty7 starts it non-login so that
/// `--rcfile` is honoured at all — and zsh is pointed at a `ZDOTDIR` that does
/// not exist, which takes every startup file the user wrote with it. Guarded,
/// such a distro gets the plain login shell it had before any of this: no
/// integration, but the startup files a bare `wsl.exe` pane reads, which is the
/// failure worth having.
///
/// `sh` parses this script, so the fish body is POSIX-quoted here — unlike the
/// SSH path, where the far end's own login shell parses the bootstrap and
/// `remote::fish_bootstrap` has to quote it the way fish reads quotes.
#[cfg_attr(not(windows), allow(dead_code))]
fn wsl_exec_script() -> String {
    format!(
        concat!(
            r#"case "${{SHELL:-}}" in "#,
            r#"*/bash) [ -r "${{TTY7_RC:-}}" ] && exec "$SHELL" --rcfile "$TTY7_RC" -i; "#,
            r#"exec "$SHELL" -l ;; "#,
            r#"*/zsh) [ -n "${{TTY7_ZDOTDIR:-}}" ] && [ -r "$TTY7_ZDOTDIR/.zshrc" ] "#,
            r#"&& export ZDOTDIR="$TTY7_ZDOTDIR"; exec "$SHELL" -l ;; "#,
            r#"*/fish) exec "$SHELL" -C {} -l ;; "#,
            r#"*) exec "${{SHELL:-/bin/sh}}" -l ;; "#,
            "esac"
        ),
        shell_quote(FISH_INTEGRATION)
    )
}

/// The files a WSL pane needs on the Windows side, and the environment that
/// tells the distro where they are.
///
/// Split out of `setup_wsl` so that everything deciding anything is compiled
/// and tested on every platform; what stays behind the `#[cfg(windows)]` is the
/// argv and a call to this.
///
/// `TTY7_USER_ZDOTDIR` is conspicuously absent, and that is the point.
/// `real_user_zdotdir` answers from this process's own environment, and this
/// process is on the Windows side of the boundary — the `ZDOTDIR` the user
/// actually set lives inside the distro and is a Linux path nothing out here
/// can name. A Windows path sent across would aim every redirector at a
/// directory that is not there, and the user's `.zshrc` would go missing
/// silently. Left unset, `zsh_redirectors` falls through to `${ZDOTDIR:-$HOME}`
/// — the distro's own home — and its `.zshenv` arm recaptures a relocated
/// `ZDOTDIR` from in there, which is the only side that ever knew it.
#[cfg_attr(not(windows), allow(dead_code))]
fn wsl_integration_env(dir: &Path, wslenv: Option<&str>) -> Option<HashMap<String, String>> {
    let rcfile = dir.join("bashrc");
    std::fs::write(&rcfile, bash_rcfile()).ok()?;

    let mut env = HashMap::new();
    let mut names = vec![format!("{WSL_RCFILE_ENV}/p")];
    env.insert(
        WSL_RCFILE_ENV.to_string(),
        rcfile.to_string_lossy().into_owned(),
    );

    // The zsh half is best-effort on purpose. It is the bash rcfile that a WSL
    // pane has depended on since this path existed, and failing the whole setup
    // because a second set of files could not be written would take a working
    // bash distro down with it. Nothing names the directory unless all of it
    // landed, so a half-written one is never advertised to the distro.
    if let Some(zdotdir) = wsl_zdotdir(dir) {
        env.insert(WSL_ZDOTDIR_ENV.to_string(), zdotdir);
        names.push(format!("{WSL_ZDOTDIR_ENV}/p"));
    }

    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    env.insert("WSLENV".to_string(), wslenv_with(wslenv, &names));
    Some(env)
}

/// The zsh redirectors, under the pane's own throwaway directory so that the
/// `remove_dir_all` closing the pane already takes them.
#[cfg_attr(not(windows), allow(dead_code))]
fn wsl_zdotdir(dir: &Path) -> Option<String> {
    let zdotdir = dir.join(format!("{ZDOTDIR_PREFIX}wsl"));
    std::fs::create_dir_all(&zdotdir).ok()?;
    for (name, contents) in zsh_redirectors() {
        std::fs::write(zdotdir.join(name), contents).ok()?;
    }
    Some(zdotdir.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn setup_wsl(args: &[String]) -> Option<Injection> {
    let distro = wsl_distro(args);
    let dir = throwaway_dir(WSLRC_PREFIX)?;
    let env = wsl_integration_env(&dir, std::env::var("WSLENV").ok().as_deref())?;

    let mut argv: Vec<String> = Vec::new();
    if let Some(d) = &distro {
        argv.push("--distribution".to_string());
        argv.push(d.clone());
    }
    if let Some(cd) = wsl_cd(args) {
        argv.push("--cd".to_string());
        argv.push(cd);
    }
    // `--` forwards the command through the distro's default shell, which makes
    // shells such as fish parse the POSIX bootstrap before `sh` can receive it.
    // `--exec` bypasses that shell and executes the bootstrap interpreter itself.
    argv.push("--exec".to_string());
    argv.push("sh".to_string());
    argv.push("-c".to_string());
    argv.push(wsl_exec_script());

    Some(Injection {
        env,
        args: argv,
        replaces_argv: true,
        dir: Some(dir),
    })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn wsl_cd(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--cd" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--cd=") {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg_attr(not(windows), allow(unused_variables))]
pub fn setup(program: Option<&str>, args: &[String], has_custom_args: bool) -> Option<Injection> {
    // Every shell defers to user-authored args, so the gate sits ahead of the
    // dispatch rather than once per arm — a shell added below inherits it
    // instead of having to remember it. Argv injection would collide with those
    // args outright, and even zsh's env-only ZDOTDIR swap changes which startup
    // files run. Arguments tty7's own detection supplied (Git Bash's `-i -l`,
    // a WSL row's `--distribution`) are not user-authored and never land here;
    // `daemon::pane::has_custom_args` is where that line is drawn.
    if has_custom_args {
        return None;
    }
    let mut injection = match shell_kind(program)? {
        ShellKind::Zsh => setup_zsh(),
        ShellKind::Fish => setup_fish(),
        ShellKind::Bash => setup_bash(),
        ShellKind::PowerShell => setup_powershell(),
        ShellKind::Nushell => setup_nushell(),
        #[cfg(windows)]
        ShellKind::Wsl => setup_wsl(args),
        #[cfg(not(windows))]
        ShellKind::Wsl => None,
    }?;

    injection
        .env
        .insert("TTY7_SHELL_INTEGRATION".to_string(), String::new());

    Some(injection)
}

pub mod remote {
    use super::{FISH_INTEGRATION, bash_rcfile, shell_quote, zsh_redirectors};

    pub(crate) const PROBE_COMMAND: &str = "echo __tty7_shell; echo $SHELL";

    const PROBE_MARKER: &str = "__tty7_shell";

    const HEREDOC: &str = "__TTY7_RC_EOF__";

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum RemoteShell {
        Zsh,
        Bash,
        Fish,
    }

    impl RemoteShell {
        fn from_path(path: &str) -> Option<Self> {
            match path.rsplit('/').next()? {
                "zsh" => Some(Self::Zsh),
                "bash" => Some(Self::Bash),
                "fish" => Some(Self::Fish),
                _ => None,
            }
        }
    }

    /// What the probe *said*, kept apart from its having said nothing.
    ///
    /// `None` means the far side never answered: the channel or the command
    /// failed, or the timeout cut the read off before the marker and the line
    /// it introduces both arrived. That is a different fact from "answered,
    /// and logs in with a shell there is no bootstrap for", which is
    /// `Some(None)`. Only the second is worth remembering — the first is a
    /// slow link or a busy server, and the next pane deserves a fresh ask.
    pub(crate) fn probe_answer(output: &str) -> Option<Option<(RemoteShell, String)>> {
        // Read exactly as far as `parse_probe` does before it can form an
        // opinion, so the two cannot disagree about whether there was one.
        let mut lines = output
            .lines()
            .map(|l| l.trim_end_matches('\r').trim())
            .skip_while(|l| *l != PROBE_MARKER);
        lines.next()?;
        lines.find(|l| !l.is_empty())?;
        Some(parse_probe(output))
    }

    pub(crate) fn parse_probe(output: &str) -> Option<(RemoteShell, String)> {
        let mut lines = output
            .lines()
            .map(|l| l.trim_end_matches('\r').trim())
            .skip_while(|l| *l != PROBE_MARKER);
        lines.next()?;
        let path = lines.find(|l| !l.is_empty())?;
        if !path.starts_with('/') {
            return None;
        }
        RemoteShell::from_path(path).map(|shell| (shell, path.to_string()))
    }

    pub(crate) fn bootstrap_command(shell: RemoteShell, shell_path: &str) -> String {
        match shell {
            RemoteShell::Zsh => zsh_bootstrap(shell_path),
            RemoteShell::Bash => bash_bootstrap(shell_path),
            RemoteShell::Fish => fish_bootstrap(shell_path),
        }
    }

    fn fish_quote(s: &str) -> String {
        format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
    }

    /// Open `$__tty7_d` as a directory this login made, or leave the block
    /// unentered.
    ///
    /// The far end sources what goes in here, and the name is
    /// `<prefix>-$$` — the remote shell's pid, which on a shared server every
    /// other account can read out of `/proc` and get to first. `mkdir -p`
    /// succeeds on a directory that is already there, so the startup files
    /// went into whatever held the name, and its owner could rewrite them
    /// before the shell read them.
    ///
    /// `mkdir` without `-p` fails when the path exists, and `-m 700` puts the
    /// mode on the creating syscall rather than a chmod after it. The caller
    /// closes the `if`; everything it writes is inside, so a name already
    /// taken means the files are never written, the `[ -s ... ]` guards that
    /// already exist see nothing, and the login falls through to the plain
    /// `exec <shell> -l` those guards were written for.
    fn open_scratch_dir(prefix: &str) -> String {
        format!(
            "__tty7_d=${{TMPDIR:-/tmp}}/{prefix}-$$\n\
             if command mkdir -m 700 \"$__tty7_d\" 2>/dev/null; then\n"
        )
    }

    fn write_file(out: &mut String, name: &str, body: &str) {
        out.push_str(&format!(
            "command cat > \"$__tty7_d/{name}\" <<'{HEREDOC}'\n{}\n{HEREDOC}\n",
            body.trim_end_matches('\n')
        ));
    }

    fn zsh_bootstrap(shell_path: &str) -> String {
        let mut out = String::new();
        out.push_str(&open_scratch_dir("tty7-zdotdir"));

        let mut guard = String::new();
        for (name, contents) in zsh_redirectors() {
            let body = if name == ".zshrc" {
                format!("{contents}{ZSH_CLEANUP_HOOK}")
            } else {
                contents
            };
            write_file(&mut out, name, &body);
            guard.push_str(&format!("[ -s \"$__tty7_d/{name}\" ] && "));
        }
        out.push_str(&format!(
            "{guard}export ZDOTDIR=\"$__tty7_d\" TTY7_RM_DIR=\"$__tty7_d\"\n"
        ));
        out.push_str("fi\n");
        out.push_str(&format!("exec {} -l\n", shell_quote(shell_path)));
        out
    }

    fn bash_bootstrap(shell_path: &str) -> String {
        let quoted = shell_quote(shell_path);
        let mut out = String::new();
        out.push_str(&open_scratch_dir("tty7-bashrc"));
        write_file(
            &mut out,
            "bashrc",
            &format!("{}{BASH_CLEANUP_HOOK}", bash_rcfile()),
        );
        out.push_str("if [ -s \"$__tty7_d/bashrc\" ]; then\n");
        out.push_str("export TTY7_RM_DIR=\"$__tty7_d\"\n");
        out.push_str(&format!("exec {quoted} --rcfile \"$__tty7_d/bashrc\" -i\n"));
        out.push_str("fi\n");
        out.push_str("fi\n");
        out.push_str(&format!("exec {quoted} -l\n"));
        out
    }

    fn fish_bootstrap(shell_path: &str) -> String {
        format!(
            "exec {} -C {} -l\n",
            fish_quote(shell_path),
            fish_quote(FISH_INTEGRATION)
        )
    }

    const ZSH_CLEANUP_HOOK: &str = r#"
# --- tty7 remote cleanup (zsh) ---
if [[ -n "$TTY7_RM_DIR" ]]; then
  typeset -g __tty7_rm_dir=$TTY7_RM_DIR
  unset TTY7_RM_DIR
  __tty7_rm_rcdir() {
    [[ -n "$__tty7_rm_dir" ]] || return 0
    command rm -rf -- "$__tty7_rm_dir"
    unset __tty7_rm_dir
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __tty7_rm_rcdir
fi
# --- end tty7 remote cleanup ---
"#;

    const BASH_CLEANUP_HOOK: &str = r#"
# --- tty7 remote cleanup (bash) ---
if [[ -n "$TTY7_RM_DIR" ]]; then
  __tty7_rm_dir=$TTY7_RM_DIR
  unset TTY7_RM_DIR
  __tty7_rm_rcdir() {
    [[ -n "$__tty7_rm_dir" ]] || return 0
    command rm -rf -- "$__tty7_rm_dir"
    unset __tty7_rm_dir
  }
  precmd_functions+=(__tty7_rm_rcdir)
fi
# --- end tty7 remote cleanup ---
"#;

    #[cfg(test)]
    mod tests {
        use super::super::{BASH_INTEGRATION, ZSH_INTEGRATION};
        use super::*;

        /// The remote bootstrap makes its scratch directory or writes nothing.
        ///
        /// The far end sources what goes in there, and the name is the remote
        /// shell's `$$` — which on a shared server every other account can
        /// read and get to first. `mkdir -p` took whatever held the name, so
        /// the startup files were written into a directory somebody else owned
        /// and could rewrite before the shell read it.
        ///
        /// Run against a planted directory, the fixed script leaves `ZDOTDIR`
        /// empty and the plant untouched; the `mkdir -p` version exported the
        /// planted path. Verified by hand that way — what is pinned here is
        /// the shape that made the difference.
        #[test]
        fn a_remote_bootstrap_creates_its_scratch_dir_rather_than_adopting_one() {
            for (shell, path) in [
                (RemoteShell::Zsh, "/bin/zsh"),
                (RemoteShell::Bash, "/bin/bash"),
            ] {
                let script = bootstrap_command(shell, path);
                assert!(
                    script.contains("if command mkdir -m 700 \"$__tty7_d\" 2>/dev/null; then"),
                    "{shell:?} does not create its own scratch directory:\n{script}"
                );
                assert!(
                    !script.contains("mkdir -p"),
                    "{shell:?} still adopts a directory that is already there:\n{script}"
                );
                // Everything written has to sit inside that `if`, or a taken
                // name means writing to `$__tty7_d` while it is somebody
                // else's — or, with the variable cleared, to `/`.
                let opened = script.find("; then").expect("the guard is there");
                let closed = script.rfind("\nfi\n").expect("the guard is closed");
                let writes = script.match_indices("command cat > ");
                for (at, _) in writes {
                    assert!(
                        at > opened && at < closed,
                        "{shell:?} writes a startup file outside the guard:\n{script}"
                    );
                }
                // And the fallback the guard drops into is still a plain login
                // shell, which is what makes refusing a taken name safe.
                assert!(
                    script.trim_end().ends_with(&format!("exec '{path}' -l")),
                    "{shell:?} has no plain login shell to fall back to:\n{script}"
                );
            }
        }

        #[test]
        fn probe_reads_the_line_after_the_marker() {
            assert_eq!(
                parse_probe("__tty7_shell\n/bin/zsh\n"),
                Some((RemoteShell::Zsh, "/bin/zsh".to_string()))
            );
            assert_eq!(
                parse_probe("Welcome to prod!\n__tty7_shell\n/usr/local/bin/fish\n"),
                Some((RemoteShell::Fish, "/usr/local/bin/fish".to_string()))
            );
            assert_eq!(
                parse_probe("__tty7_shell\r\n/bin/bash\r\n"),
                Some((RemoteShell::Bash, "/bin/bash".to_string()))
            );
        }

        #[test]
        fn probe_declines_anything_that_isnt_a_shell_we_know() {
            assert_eq!(parse_probe("__tty7_shell\n$SHELL\n"), None);
            assert_eq!(parse_probe("__tty7_shell\n\n"), None);
            assert_eq!(parse_probe("__tty7_shell\n/bin/ksh\n"), None);
            assert_eq!(parse_probe("/bin/zsh\n"), None);
        }

        #[test]
        fn a_probe_that_never_answered_is_told_apart_from_one_that_said_no() {
            // Answered, with a shell there is a bootstrap for.
            assert_eq!(
                probe_answer("__tty7_shell\n/bin/zsh\n"),
                Some(Some((RemoteShell::Zsh, "/bin/zsh".to_string())))
            );
            // Answered, and the answer is one there is no bootstrap for. Worth
            // remembering: asking again gets the same `/bin/ksh` every time.
            assert_eq!(probe_answer("__tty7_shell\n/bin/ksh\n"), Some(None));
            assert_eq!(probe_answer("__tty7_shell\n$SHELL\n"), Some(None));

            // Never answered. The channel or the exec failed, or the read timed
            // out — none of which says anything about the remote's shell, and
            // all of which used to be cached as if it did.
            assert_eq!(probe_answer(""), None, "nothing came back at all");
            assert_eq!(
                probe_answer("Welcome to prod!\n"),
                None,
                "a banner, and then the read was cut off"
            );
            assert_eq!(
                probe_answer("__tty7_shell\n"),
                None,
                "the marker arrived and the line it introduces did not"
            );
            assert_eq!(
                probe_answer("__tty7_shell\n\n"),
                None,
                "and a blank line is not that line"
            );
        }

        #[test]
        fn zsh_bootstrap_gates_zdotdir_on_every_redirector_landing() {
            let script = bootstrap_command(RemoteShell::Zsh, "/bin/zsh");
            for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
                assert!(
                    script.contains(&format!("[ -s \"$__tty7_d/{name}\" ] &&")),
                    "missing landing check for {name}"
                );
            }
            let export = script.find("export ZDOTDIR=").expect("exports ZDOTDIR");
            let exec = script.find("exec '/bin/zsh' -l").expect("execs zsh");
            assert!(export < exec);
            assert!(script.contains("__tty7_report_cwd"));
        }

        #[test]
        fn file_writing_bootstraps_end_in_a_bare_exec_of_the_users_shell() {
            for (shell, path) in [
                (RemoteShell::Zsh, "/bin/zsh"),
                (RemoteShell::Bash, "/bin/bash"),
            ] {
                let script = bootstrap_command(shell, path);
                let last = script.trim_end().lines().last().unwrap();
                assert_eq!(
                    last,
                    format!("exec '{path}' -l"),
                    "{shell:?} bootstrap must end by exec'ing {path} bare"
                );
            }
        }

        #[test]
        fn bash_bootstrap_forces_a_non_login_shell_through_the_rcfile() {
            let script = bootstrap_command(RemoteShell::Bash, "/bin/bash");
            assert!(script.contains("exec '/bin/bash' --rcfile \"$__tty7_d/bashrc\" -i"));
            assert!(script.contains("source /etc/profile"));
        }

        #[test]
        fn fish_bootstrap_is_one_exec_carrying_the_escaped_body() {
            let script = bootstrap_command(RemoteShell::Fish, "/usr/bin/fish");
            assert!(script.starts_with("exec '/usr/bin/fish' -C '"));
            assert!(script.trim_end().ends_with("' -l"));
            assert!(!script.contains("mkdir"));

            assert!(script.contains(r"printf \'\\e]%s\\a\' $argv[1]"));
        }

        #[test]
        fn quoting_survives_paths_and_bodies_that_fight_back() {
            assert_eq!(shell_quote("/o'dd/zsh"), r"'/o'\''dd/zsh'");
            assert_eq!(fish_quote(r"a'b\c"), r"'a\'b\\c'");
        }

        #[test]
        fn heredoc_delimiter_cannot_appear_in_a_body_it_delimits() {
            for body in [ZSH_INTEGRATION, BASH_INTEGRATION, FISH_INTEGRATION] {
                assert!(!body.contains(HEREDOC));
            }
            assert!(!bash_rcfile().contains(HEREDOC));
        }

        #[cfg(unix)]
        fn parse_check(
            shell: &str,
            syntax_only_flag: &str,
            script: &str,
        ) -> Option<(bool, String)> {
            use std::io::Write as _;
            use std::process::{Command, Stdio};

            let mut child = Command::new(shell)
                .arg(syntax_only_flag)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .ok()?;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(script.as_bytes())
                .expect("write script");
            let out = child.wait_with_output().expect("wait for parse check");
            Some((
                out.status.success(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ))
        }

        #[cfg(unix)]
        #[test]
        fn bootstrap_scripts_parse_under_their_real_shells() {
            let cases = [
                (RemoteShell::Zsh, "zsh", "-n", "/bin/zsh"),
                (RemoteShell::Bash, "bash", "-n", "/bin/bash"),
                (RemoteShell::Fish, "fish", "--no-execute", "/usr/bin/fish"),
            ];
            for (shell, bin, flag, path) in cases {
                let script = bootstrap_command(shell, path);
                if let Some((ok, stderr)) = parse_check(bin, flag, &script) {
                    assert!(ok, "{bin} rejected its bootstrap script:\n{stderr}");
                }
            }
        }

        #[cfg(unix)]
        #[test]
        fn heredoc_bodies_parse_under_their_real_shells() {
            for (name, contents) in zsh_redirectors() {
                let body = if name == ".zshrc" {
                    format!("{contents}{ZSH_CLEANUP_HOOK}")
                } else {
                    contents
                };
                if let Some((ok, stderr)) = parse_check("zsh", "-n", &body) {
                    assert!(ok, "zsh rejected the remote {name}:\n{stderr}");
                }
            }
            let rcfile = format!("{}{BASH_CLEANUP_HOOK}", bash_rcfile());
            if let Some((ok, stderr)) = parse_check("bash", "-n", &rcfile) {
                assert!(ok, "bash rejected the remote rcfile:\n{stderr}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_mode_detection_survives_rebound_escape_and_inputrc() {
        assert!(
            ZSH_INTEGRATION.contains("bindkey -lL main"),
            "zsh edit-mode detection must key off the main keymap link"
        );
        assert!(ZSH_INTEGRATION.contains("viins"));
        assert!(
            BASH_INTEGRATION.contains("editing-mode vi"),
            "bash edit-mode detection must read readline's mode via bind -v"
        );
    }

    #[test]
    fn is_our_zdotdir_matches_only_our_prefix() {
        assert!(is_our_zdotdir("/tmp/tty7-zdotdir-1234-0"));
        assert!(is_our_zdotdir("tty7-zdotdir-x"));
        assert!(!is_our_zdotdir("/home/alice/.config/zsh"));
        assert!(!is_our_zdotdir("/tmp/other-zdotdir"));
        assert!(!is_our_zdotdir(""));
        assert!(!is_our_zdotdir("/tmp/not-tty7-zdotdir-1"));
    }

    /// `\e]133;D;1\a` — what a shell reports for the `false` these tests type.
    ///
    /// The trailing BEL is the point. `contains("133;D;1")` is a prefix match on
    /// the exit status, so it also accepts `133;D;127` (the bootstrap exec'd a
    /// shell that could not find `false`) and `133;D;130` (interrupted) — a
    /// bootstrap that never ran the command at all would pass every assertion
    /// below. Matching through the terminator pins the status whole.
    const FAILED_COMMAND_MARK: &str = "133;D;1\u{7}";

    /// Type `keys` at a freshly spawned shell and return everything it wrote back.
    ///
    /// `keys` carries its own Enter because the two families disagree about which
    /// byte that is. A line-discipline shell gets `\n` (the pty's ICRNL would
    /// accept `\r` too); PSReadLine puts the tty in raw mode and reads keys
    /// itself, so only a real `\r` submits — `\n` just sits in the buffer and the
    /// command never runs.
    fn prompt_cycle_over_pty(
        program: &str,
        injection: &Injection,
        keys: &[u8],
        cwd: Option<&Path>,
    ) -> String {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::io::{Read, Write};

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(program);
        cmd.args(&injection.args);
        for (k, v) in &injection.env {
            cmd.env(k, v);
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let mut child = pty.slave.spawn_command(cmd).expect("spawn shell");

        let mut writer = pty.master.take_writer().expect("writer");
        let mut reader = pty.master.try_clone_reader().expect("reader");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut out = Vec::new();
        let mut answered = 0usize;
        let mut typed = false;
        let mut seen_fail = None;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let text = String::from_utf8_lossy(&out);

            // Play terminal for the one query that blocks: PSReadLine asks where
            // the cursor is (`CSI 6n`) before it will draw anything, and waits for
            // the answer. Nothing is behind this pty to give one, so an unanswered
            // report means pwsh never echoes a keystroke and the whole test times
            // out. The position itself does not matter here — only that a reply
            // arrives. A line-discipline shell never asks, so this is inert for
            // the Git Bash and WSL cases.
            let asked = text.matches("\u{1b}[6n").count();
            if asked > answered {
                for _ in answered..asked {
                    let _ = writer.write_all(b"\x1b[1;1R");
                }
                let _ = writer.flush();
                answered = asked;
            }

            // Type only once the first prompt is marked (A or B — they arrive
            // in the same prompt cycle for every integrated shell, and a
            // Nushell without a config-defined prompt_indicator emits only A).
            // Writing at spawn time instead puts the keystrokes ahead of the
            // cursor-position reply in the same input stream, and pwsh — still
            // waiting on that reply — eats `false\r` as the answer to its own
            // query. The command then never runs, and the failure reads as a
            // missing C mark rather than as the race it is.
            if !typed && (text.contains("133;A") || text.contains("133;B")) {
                writer.write_all(keys).expect("write");
                writer.flush().expect("flush");
                typed = true;
            }

            // Stop only once the typed command's own D report lands. A shell can
            // emit an unpaired D while drawing its first prompt, and breaking on
            // that would sample the transcript before the prompt cycle under test
            // has run at all.
            if typed && text.contains(FAILED_COMMAND_MARK) && seen_fail.is_none() {
                seen_fail = Some(std::time::Instant::now());
            }
            // The D mark and the rest of the same prompt cycle (the cwd report,
            // the A mark) are separate writes: breaking the moment the mark
            // arrives can sample the transcript before the report does. Hold
            // the pty open briefly so the cycle's tail lands.
            if let Some(at) = seen_fail
                && at.elapsed() >= std::time::Duration::from_millis(500)
            {
                break;
            }
        }
        // Reap before asserting: a panic here would otherwise leave the shell
        // holding the pty open for the rest of the test run.
        let _ = child.kill();
        let _ = child.wait();
        drop(pty.master);
        assert!(
            typed,
            "no prompt-start mark ever arrived, so nothing was typed; got:\n{}",
            String::from_utf8_lossy(&out)
        );
        String::from_utf8_lossy(&out).into_owned()
    }

    fn reported_cwd(text: &str) -> PathBuf {
        let payload = text
            .split("\u{1b}]")
            .find(|s| s.starts_with("7;file://"))
            .and_then(|s| s.split(['\u{7}', '\u{1b}']).next())
            .unwrap_or_else(|| panic!("expected OSC 7; got:\n{text}"));
        crate::daemon::pane::parse_osc7(payload.as_bytes())
            .unwrap_or_else(|| panic!("daemon could not parse OSC 7 payload {payload:?}"))
    }

    /// The last OSC 7 cwd report in a transcript — the one emitted after the
    /// typed commands ran, which is what proves `cd` moved the pane's cwd.
    fn last_osc7(text: &str) -> PathBuf {
        let payload = text
            .split("\u{1b}]")
            .filter(|s| s.starts_with("7;file://"))
            .last()
            .and_then(|s| s.split(['\u{7}', '\u{1b}']).next())
            .unwrap_or_else(|| panic!("expected OSC 7; got:\n{text}"));
        crate::daemon::pane::parse_osc7(payload.as_bytes())
            .unwrap_or_else(|| panic!("daemon could not parse OSC 7 payload {payload:?}"))
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_reports_the_full_prompt_cycle_over_a_real_pty() {
        let Some(bash) = crate::core::shells::git_bash_path() else {
            eprintln!("skipping: Git for Windows not installed");
            return;
        };
        let bash = bash.to_string_lossy().into_owned();
        let injection = setup(Some(&bash), &[], false).expect("bash integration");
        let text = prompt_cycle_over_pty(&bash, &injection, b"false\n", None);

        for mark in ["133;A", "133;B", "133;C", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "Git Bash must report {mark:?}; got:\n{text}"
            );
        }
        let cwd = reported_cwd(&text);
        assert!(
            cwd.exists(),
            "Git Bash reported a cwd the Windows side cannot resolve: {cwd:?} \
             — a drive-relative msys path, so `pwd -W` translation regressed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn wsl_reports_the_full_prompt_cycle_over_a_real_pty() {
        let Some(distro) = crate::core::shells::wsl_distros().into_iter().next() else {
            eprintln!("skipping: no WSL distributions installed");
            return;
        };
        let args: Vec<String> = vec![
            "--distribution".into(),
            distro.clone(),
            "--cd".into(),
            "~".into(),
        ];
        let injection = setup(Some("wsl.exe"), &args, false).expect("wsl integration");
        let text = prompt_cycle_over_pty("wsl.exe", &injection, b"false\n", None);

        for mark in ["133;A", "133;B", "133;C", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "WSL ({distro}) must report {mark:?}; got:\n{text}"
            );
        }
        let cwd = reported_cwd(&text);
        assert!(
            cwd.to_string_lossy().starts_with('/'),
            "expected the distro's own absolute path, got {cwd:?}"
        );
    }

    /// The OSC 0 title the shell settled on, as `user@host:dir`.
    fn reported_title(text: &str) -> String {
        text.split("\u{1b}]")
            .filter_map(|s| s.strip_prefix("0;"))
            .filter_map(|s| s.split(['\u{7}', '\u{1b}']).next())
            // pwsh emits an empty OSC 0 of its own just before ours; the one
            // under test is whatever is left after dropping those.
            .filter(|t| !t.is_empty())
            .last()
            .unwrap_or_else(|| panic!("expected a non-empty OSC 0 title; got:\n{text}"))
            .to_string()
    }

    #[cfg(unix)]
    fn pwsh_on_path() -> Option<String> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("pwsh"))
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Guards the Unix half of the PowerShell integration, which had no coverage
    /// at all until #583 — every pty round-trip here used to be `#[cfg(windows)]`,
    /// so a script written against `$env:USERNAME` / `$env:COMPUTERNAME` /
    /// `$env:USERPROFILE` shipped for two platforms where all three are empty.
    /// zsh over a real pty — the shell most of this project's users are in.
    ///
    /// Four shells had a pty test: git-bash and WSL on Windows, pwsh, and
    /// nushell. Not zsh, which is the macOS default, and not bash. So the
    /// route almost every user takes — ZDOTDIR pointed at a scratch directory
    /// of redirectors, each sourcing the user's own file and then adding the
    /// hooks — was exercised only by unit tests of the strings it writes.
    ///
    /// Through `setup`, never `setup_zsh`: the empty-sentinel reset lives in
    /// `setup`, and this suite runs inside a tty7 pane where
    /// `TTY7_SHELL_INTEGRATION=1` is already in the environment. Reaching for
    /// the builder directly would leave every redirector guarding itself off,
    /// and the assertions below would then be passing on marks stock zsh
    /// emits by itself.
    #[cfg(unix)]
    #[test]
    fn zsh_reports_the_full_prompt_cycle_over_a_real_pty() {
        let zsh = ["/bin/zsh", "/usr/bin/zsh", "/usr/local/bin/zsh"]
            .into_iter()
            .find(|p| Path::new(p).exists());
        let Some(zsh) = zsh else {
            eprintln!("skipping: no zsh on this machine");
            return;
        };
        let injection = setup(Some(zsh), &[], false).expect("zsh integration");
        assert_eq!(
            injection
                .env
                .get("TTY7_SHELL_INTEGRATION")
                .map(String::as_str),
            Some(""),
            "without the reset the redirectors guard themselves off and this \
             test would pass on marks zsh emits on its own"
        );

        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let text = prompt_cycle_over_pty(zsh, &injection, b"false\r", Some(&home));

        for mark in ["133;A", "133;B", "133;C", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "zsh must report {mark:?}; got:\n{text}"
            );
        }
        assert_eq!(
            reported_cwd(&text),
            home.canonicalize().unwrap_or(home.clone()),
            "OSC 7 must name the pane's real cwd"
        );
        // No OSC 0 title is asserted, and none should be: of the five
        // integrations only PowerShell's writes one, because PowerShell has no
        // convention of its own — `powershell_integration_sets_an_osc_title`
        // pins that. On a POSIX shell the title belongs to the user's prompt,
        // and tty7 names the pane from OSC 7 instead. This test was written
        // expecting a title and was wrong to; the marks above are what zsh
        // actually undertakes to send.
        assert!(
            !text.contains("\u{1b}]0;"),
            "zsh's integration has started setting a title; if that is meant, \
             the pane's own naming has to agree with it:\n{text}"
        );
    }

    /// bash over a real pty, which reaches the shell a different way.
    ///
    /// zsh is steered with `ZDOTDIR` in the environment; bash is steered with
    /// `--rcfile … -i` on the command line, and `replaces_argv` is true for it
    /// alone. Those are separate mechanisms with separate ways to break — a
    /// dropped `-i` leaves a shell that runs the rc and exits, and a bad
    /// `--rcfile` path leaves a shell with no integration and no complaint.
    /// Neither had ever been run.
    ///
    /// Through `setup` for the reason
    /// `zsh_reports_the_full_prompt_cycle_over_a_real_pty` gives: the empty
    /// sentinel is what stops the injected rc guarding itself off inside a
    /// tty7 pane.
    #[cfg(unix)]
    #[test]
    fn bash_reports_the_full_prompt_cycle_over_a_real_pty() {
        let bash = [
            "/bin/bash",
            "/usr/bin/bash",
            "/usr/local/bin/bash",
            "/opt/homebrew/bin/bash",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists());
        let Some(bash) = bash else {
            eprintln!("skipping: no bash on this machine");
            return;
        };
        let injection = setup(Some(bash), &[], false).expect("bash integration");
        assert_eq!(
            injection
                .env
                .get("TTY7_SHELL_INTEGRATION")
                .map(String::as_str),
            Some(""),
            "without the reset the injected rc guards itself off and this test \
             would pass on whatever bash prints by itself"
        );
        assert!(
            injection.args.iter().any(|a| a == "-i"),
            "bash needs -i or it runs the rc and exits: {:?}",
            injection.args
        );

        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let text = prompt_cycle_over_pty(bash, &injection, b"false\r", Some(&home));

        for mark in ["133;A", "133;B", "133;C", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "bash must report {mark:?}; got:\n{text}"
            );
        }
        assert_eq!(
            reported_cwd(&text),
            home.canonicalize().unwrap_or(home.clone()),
            "OSC 7 must name the pane's real cwd"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pwsh_reports_the_full_prompt_cycle_over_a_real_pty_on_unix() {
        let Some(pwsh) = pwsh_on_path() else {
            eprintln!("skipping: pwsh not installed");
            return;
        };
        let injection = setup(Some(&pwsh), &[], false).expect("powershell integration");
        // From $HOME, so the title's `~` abbreviation is exercised rather than
        // assumed — it is the half that silently did nothing on Unix.
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let text = prompt_cycle_over_pty(&pwsh, &injection, b"false\r", Some(&home));

        for mark in ["133;A", "133;B", "133;C", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "pwsh must report {mark:?}; got:\n{text}"
            );
        }

        let cwd = reported_cwd(&text);
        assert_eq!(
            cwd,
            home.canonicalize().unwrap_or(home.clone()),
            "OSC 7 must name the pane's real cwd"
        );

        // Asserted by shape, not against $USER: a CI runner does not reliably
        // export it, and the bug being guarded is emptiness — `@:` — not which
        // name lands there. When the environment does name the user, pin it.
        let title = reported_title(&text);
        let (who, path) = title.split_once(':').expect("title is `user@host:dir`");
        let (user, host) = who.split_once('@').expect("title is `user@host:dir`");
        assert!(
            !user.is_empty(),
            "the title must name the user — `$env:USERNAME` is empty on Unix, \
             which is what left it starting with a bare `@`; got {title:?}"
        );
        assert!(
            !host.is_empty(),
            "the title must name the host — `$env:COMPUTERNAME` is empty on Unix; \
             got {title:?}"
        );
        if let Ok(expected) = std::env::var("USER") {
            assert_eq!(user, expected, "the title named the wrong user");
        }
        assert_eq!(
            path, "~",
            "$HOME must abbreviate to `~` — the old check keyed off \
             `$env:USERPROFILE`, which does not exist on Unix; got {title:?}"
        );
    }

    #[test]
    fn shell_kind_maps_known_basenames() {
        assert!(matches!(shell_kind(Some("/bin/zsh")), Some(ShellKind::Zsh)));
        assert!(matches!(shell_kind(Some("zsh")), Some(ShellKind::Zsh)));
        assert!(matches!(
            shell_kind(Some("/bin/bash")),
            Some(ShellKind::Bash)
        ));
        assert!(matches!(
            shell_kind(Some("/usr/local/bin/fish")),
            Some(ShellKind::Fish)
        ));
        for prog in [
            "powershell.exe",
            "powershell",
            "pwsh",
            "pwsh.exe",
            "C:/Program Files/PowerShell/7/pwsh.exe",
            "PowerShell.EXE",
        ] {
            assert!(
                matches!(shell_kind(Some(prog)), Some(ShellKind::PowerShell)),
                "{prog} should map to PowerShell"
            );
        }
        for prog in ["nu", "nu.exe", "C:/Tools/nu.exe"] {
            assert!(
                matches!(shell_kind(Some(prog)), Some(ShellKind::Nushell)),
                "{prog} should map to Nushell"
            );
        }
        assert!(shell_kind(Some("/bin/sh")).is_none());
        assert!(shell_kind(Some("cmd.exe")).is_none());
        assert!(matches!(shell_kind(Some("wsl.exe")), Some(ShellKind::Wsl)));
        assert!(matches!(shell_kind(Some("wsl")), Some(ShellKind::Wsl)));
    }

    #[cfg(windows)]
    #[test]
    fn wsl_setup_never_contacts_the_distro() {
        let args: Vec<String> = vec![
            "--distribution".into(),
            "tty7-no-such-distro-exists".into(),
            "--cd".into(),
            "~".into(),
        ];
        let inj = setup(Some("wsl.exe"), &args, false)
            .expect("setup must not depend on reaching the distro");

        let sep = inj
            .args
            .iter()
            .position(|a| a == "--exec")
            .expect("`--exec`");
        assert_eq!(
            &inj.args[..sep],
            &[
                "--distribution".to_string(),
                "tty7-no-such-distro-exists".to_string(),
                "--cd".to_string(),
                "~".to_string()
            ]
        );
        assert_eq!(inj.args[sep + 1], "sh");
        assert_eq!(inj.args[sep + 2], "-c");
        assert!(inj.args[sep + 3].contains("$SHELL"));
        assert!(inj.args[sep + 3].contains("--rcfile"));
        assert!(inj.replaces_argv);
    }

    #[test]
    fn the_wsl_bootstrap_carries_integration_for_fish_not_just_bash() {
        let script = wsl_exec_script();

        assert!(script.contains(
            r#"*/bash) [ -r "${TTY7_RC:-}" ] && exec "$SHELL" --rcfile "$TTY7_RC" -i; exec "$SHELL" -l ;;"#
        ));
        assert!(script.contains(&format!(
            r#"*/fish) exec "$SHELL" -C {} -l ;;"#,
            shell_quote(FISH_INTEGRATION)
        )));

        // `sh` parses this script, so the body is POSIX-quoted — backslashes
        // pass through untouched. `remote::fish_quote`, which the SSH path uses
        // because the far end's own fish parses the bootstrap there, would
        // double every one of them and hand fish `\\e]%s\\a`.
        assert!(script.contains(r"printf '\''\e]%s\a'\'' $argv[1]"));
    }

    #[test]
    fn the_wsl_bootstrap_carries_integration_for_zsh_too() {
        let script = wsl_exec_script();

        assert!(
            script
                .contains(r#"*/zsh) [ -n "${TTY7_ZDOTDIR:-}" ] && [ -r "$TTY7_ZDOTDIR/.zshrc" ] "#)
        );
        assert!(script.contains(r#"&& export ZDOTDIR="$TTY7_ZDOTDIR"; exec "$SHELL" -l ;;"#));

        // The redirectors read this to find the user's own startup files, and
        // only the distro can answer it. Naming it out here would aim them at a
        // Windows path no distro has.
        assert!(
            !script.contains("TTY7_USER_ZDOTDIR"),
            "the bootstrap must not carry a ZDOTDIR this side made up"
        );
    }

    /// A shell's scratch directory is made by this process, at 0700, or not
    /// used at all.
    ///
    /// The files in it are sourced by the shell. The name is predictable —
    /// `<prefix><pid>-<seq>`, which the reaper reads — and on Linux the system
    /// temp directory is `/tmp`, mode 1777, so the name is one any account can
    /// take first. `create_dir_all` adopted whatever was there; a directory
    /// somebody else owns is one they can rewrite `.zshrc` in between tty7
    /// writing it and zsh reading it.
    #[cfg(unix)]
    #[test]
    fn a_shell_scratch_directory_is_created_by_us_and_closed_to_others() {
        use std::os::unix::fs::PermissionsExt as _;

        let first = throwaway_dir("tty7-scratchtest-").expect("a scratch directory");
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o7777,
            0o700,
            "the shell's startup files are readable by other accounts"
        );

        // A name already taken is stepped over, not adopted: this is what an
        // attacker who guessed the pid and sequence leaves behind.
        let squatted = first.with_file_name(format!("tty7-squattest-{}-0", std::process::id()));
        std::fs::create_dir_all(&squatted).unwrap();
        std::fs::set_permissions(&squatted, std::fs::Permissions::from_mode(0o777)).unwrap();
        let got = throwaway_dir("tty7-squattest-").expect("a scratch directory");
        assert_ne!(
            got, squatted,
            "the directory somebody else had already made was used anyway"
        );
        assert_eq!(
            std::fs::metadata(&squatted).unwrap().permissions().mode() & 0o7777,
            0o777,
            "somebody else's directory was re-permissioned instead of left alone"
        );

        // And the reaper still reads the owner out of the names it hands back.
        for dir in [&first, &got] {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.contains(&format!("{}-", std::process::id())),
                "{name} no longer carries the pid the reaper matches on"
            );
        }

        for dir in [&first, &got, &squatted] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn wsl_integration_env_writes_what_each_shell_reads_and_names_it_for_the_distro() {
        let dir = throwaway_dir("tty7-wsltest-").expect("temp dir");
        let env = wsl_integration_env(&dir, None).expect("integration files");

        let rcfile = PathBuf::from(env.get(WSL_RCFILE_ENV).expect("TTY7_RC"));
        assert_eq!(std::fs::read_to_string(&rcfile).unwrap(), bash_rcfile());

        let zdotdir = PathBuf::from(env.get(WSL_ZDOTDIR_ENV).expect("TTY7_ZDOTDIR"));
        for (name, contents) in zsh_redirectors() {
            assert_eq!(
                std::fs::read_to_string(zdotdir.join(name)).unwrap(),
                contents
            );
        }

        // Both are Windows paths — `/p` is what turns them into the distro's
        // view of themselves, so neither may be pre-translated here.
        assert_eq!(env.get("WSLENV").unwrap(), "TTY7_RC/p:TTY7_ZDOTDIR/p");
        assert!(!env.contains_key("TTY7_USER_ZDOTDIR"));
        assert!(!env.contains_key("ZDOTDIR"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The arms are strings until something runs them. This asks a real `sh`
    /// which one it picks, with stub shells standing in for the distro's.
    #[cfg(unix)]
    #[test]
    fn the_wsl_bootstrap_picks_the_arm_that_matches_the_distro_s_shell() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Command;

        let dir = throwaway_dir("tty7-wslarm-").expect("temp dir");
        for name in ["bash", "zsh", "fish"] {
            let stub = dir.join(name);
            std::fs::write(
                &stub,
                format!("#!/bin/sh\necho {name} \"$@\"\necho ZDOTDIR=${{ZDOTDIR-unset}}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let zdotdir = dir.join("zdot");
        std::fs::create_dir_all(&zdotdir).unwrap();
        std::fs::write(zdotdir.join(".zshrc"), "").unwrap();

        let rcfile = dir.join("bashrc");
        std::fs::write(&rcfile, "").unwrap();

        let run = |shell: &str, rc: &str, zdot: &str| {
            let out = Command::new("sh")
                .arg("-c")
                .arg(wsl_exec_script())
                .env_clear()
                .env("SHELL", dir.join(shell).to_string_lossy().into_owned())
                .env("TTY7_RC", rc)
                .env("TTY7_ZDOTDIR", zdot)
                .output()
                .expect("run the bootstrap");
            assert!(out.status.success(), "the bootstrap exited non-zero");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        let rc = rcfile.to_string_lossy().into_owned();
        let zdot = zdotdir.to_string_lossy().into_owned();
        assert!(run("bash", &rc, &zdot).contains("bash --rcfile"));
        assert!(run("fish", &rc, &zdot).starts_with("fish -C"));

        let zsh = run("zsh", &rc, &zdot);
        assert!(zsh.contains("zsh -l"), "{zsh}");
        assert!(zsh.contains(&format!("ZDOTDIR={zdot}")), "{zsh}");

        // The distro that cannot see /mnt. Every arm that was handed a path has
        // to notice, and fall back to the login shell that reads the user's own
        // files rather than to one holding a path that is not there.
        let missing = dir.join("not-there").to_string_lossy().into_owned();

        let blind_zsh = run("zsh", &rc, &missing);
        assert!(blind_zsh.contains("zsh -l"), "{blind_zsh}");
        assert!(blind_zsh.contains("ZDOTDIR=unset"), "{blind_zsh}");

        let blind_bash = run("bash", &missing, &zdot);
        assert!(
            !blind_bash.contains("--rcfile"),
            "bash was handed an rcfile it cannot read: {blind_bash}"
        );
        assert!(blind_bash.contains("bash -l"), "{blind_bash}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The distro runs this through `sh`, so a quoting slip is a pane that
    /// never opens. CI's Linux and macOS legs have a real `sh` to ask.
    #[cfg(unix)]
    #[test]
    fn the_wsl_bootstrap_is_valid_posix_sh() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh -n");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(wsl_exec_script().as_bytes())
            .expect("write script");
        let out = child.wait_with_output().expect("wait for sh -n");
        assert!(
            out.status.success(),
            "sh rejected the WSL bootstrap:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn wsl_distro_and_cd_are_read_from_either_flag_spelling() {
        let long: Vec<String> = ["--distribution", "Ubuntu-24.04", "--cd", "~"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(wsl_distro(&long).as_deref(), Some("Ubuntu-24.04"));
        assert_eq!(wsl_cd(&long).as_deref(), Some("~"));

        let short: Vec<String> = ["-d", "Debian"].iter().map(|s| s.to_string()).collect();
        assert_eq!(wsl_distro(&short).as_deref(), Some("Debian"));
        assert_eq!(wsl_cd(&short), None);

        let eq: Vec<String> = ["--distribution=Arch", "--cd=/tmp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(wsl_distro(&eq).as_deref(), Some("Arch"));
        assert_eq!(wsl_cd(&eq).as_deref(), Some("/tmp"));

        assert_eq!(wsl_distro(&[]), None);
        assert_eq!(wsl_distro(&["--distribution".to_string()]), None);
    }

    #[test]
    fn wslenv_preserves_the_users_own_entries() {
        assert_eq!(
            wslenv_with(Some("MYVAR/p:OTHER"), &["TTY7_RC/p"]),
            "MYVAR/p:OTHER:TTY7_RC/p"
        );
        assert_eq!(wslenv_with(None, &["TTY7_RC/p"]), "TTY7_RC/p");
        assert_eq!(wslenv_with(Some(""), &["TTY7_RC/p"]), "TTY7_RC/p");
        assert_eq!(wslenv_with(Some("TTY7_RC/l"), &["TTY7_RC/p"]), "TTY7_RC/l");
        assert_eq!(
            wslenv_with(None, &["TTY7_RC/p", "TTY7_ZDOTDIR/p"]),
            "TTY7_RC/p:TTY7_ZDOTDIR/p"
        );
        assert_eq!(
            wslenv_with(Some("TTY7_ZDOTDIR/l"), &["TTY7_RC/p", "TTY7_ZDOTDIR/p"]),
            "TTY7_ZDOTDIR/l:TTY7_RC/p"
        );
    }

    #[test]
    fn shell_kind_strips_exe_for_non_powershell_shells() {
        for prog in [
            "C:/Program Files/Git/bin/bash.exe",
            "C:/msys64/usr/bin/bash.exe",
        ] {
            assert!(
                matches!(shell_kind(Some(prog)), Some(ShellKind::Bash)),
                "{prog} should map to Bash"
            );
        }
        if !cfg!(windows) {
            for prog in ["bash.exe", "BASH.EXE", "bash"] {
                assert!(matches!(shell_kind(Some(prog)), Some(ShellKind::Bash)));
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn shell_kind_declines_the_wsl_bash_launcher() {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        for prog in [
            format!(r"{system_root}\System32\bash.exe"),
            format!(r"{system_root}/System32/bash.exe"),
            format!(r"{system_root}\SysWOW64\bash.exe"),
            format!(r"{system_root}\system32\BASH.EXE"),
        ] {
            assert!(
                shell_kind(Some(&prog)).is_none(),
                "{prog} is the WSL launcher and must not be treated as Bash"
            );
        }
        for prog in ["bash", "bash.exe", "BASH.EXE"] {
            assert!(
                shell_kind(Some(prog)).is_none(),
                "{prog} cannot be identified as msys bash and must be declined"
            );
        }
    }

    #[test]
    fn bash_rcfile_path_uses_forward_slashes_on_windows() {
        let rendered = bash_path(Path::new(
            r"C:\Users\a\AppData\Local\Temp\tty7-bashrc-1-0\bashrc",
        ));
        if cfg!(windows) {
            assert_eq!(
                rendered,
                "C:/Users/a/AppData/Local/Temp/tty7-bashrc-1-0/bashrc"
            );
        }
        assert_eq!(
            bash_path(Path::new("/tmp/tty7-bashrc-1-0/bashrc")),
            "/tmp/tty7-bashrc-1-0/bashrc"
        );
    }

    #[test]
    fn zsh_redirectors_source_user_files_and_append_integration() {
        let files = zsh_redirectors();
        assert_eq!(files.len(), 4);
        let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, [".zshenv", ".zprofile", ".zshrc", ".zlogin"]);
        for (name, body) in &files {
            assert!(
                body.contains("$TTY7_USER_ZDOTDIR"),
                "{name} should reference the user's real ZDOTDIR"
            );
            assert!(body.contains(name), "{name} should source its own name");
            assert!(body.contains("source"), "{name} should source");
        }
        let zshrc = &files[2].1;
        assert!(zshrc.contains("__tty7_precmd"));
        assert!(zshrc.contains("133;A"));
        assert!(!files[0].1.contains("__tty7_precmd"));
    }

    #[test]
    fn zsh_redirectors_point_zdotdir_at_the_real_dir_only_while_sourcing() {
        for (name, body) in zsh_redirectors() {
            let save = body.find("__tty7_ztmp=$ZDOTDIR").expect("stashes our dir");
            let aim = body
                .find("ZDOTDIR=$TTY7_USER_ZDOTDIR")
                .expect("aims at the real dir");
            let source = body
                .find(&format!("source \"${{ZDOTDIR:-$HOME}}/{name}\""))
                .expect("sources the user's file");
            let restore = body
                .rfind("ZDOTDIR=$__tty7_ztmp")
                .expect("restores our dir");
            let unset = body
                .rfind("unset __tty7_ztmp")
                .expect("cleans up its scratch var");
            assert!(
                save < aim && aim < source && source < restore && restore < unset,
                "{name}: order must be stash → aim-at-real → source → restore-ours → unset"
            );
        }
    }

    #[test]
    fn zshenv_recaptures_a_user_relocated_zdotdir() {
        let files = zsh_redirectors();
        let zshenv = &files[0].1;
        let source = zshenv.find("source \"${ZDOTDIR:-$HOME}/.zshenv\"").unwrap();
        let recapture = zshenv
            .find("export TTY7_USER_ZDOTDIR=${ZDOTDIR:-$HOME}")
            .unwrap();
        let restore = zshenv.rfind("ZDOTDIR=$__tty7_ztmp").unwrap();
        assert!(
            source < recapture && recapture < restore,
            "recapture must run after sourcing the user's .zshenv, before we restore our dir"
        );
        for (name, body) in &files[1..] {
            assert!(
                !body.contains("export TTY7_USER_ZDOTDIR"),
                "{name} must not re-export TTY7_USER_ZDOTDIR"
            );
        }
    }

    #[test]
    fn zsh_integration_restores_real_zdotdir_after_startup() {
        assert!(ZSH_INTEGRATION.contains("__tty7_restore_zdotdir"));
        assert!(ZSH_INTEGRATION.contains("ZDOTDIR=${TTY7_USER_ZDOTDIR:-$HOME}"));
        assert!(
            ZSH_INTEGRATION.contains("add-zsh-hook -d precmd __tty7_restore_zdotdir"),
            "the restore hook must deregister itself so it runs exactly once"
        );
    }

    #[test]
    fn bash_rcfile_sources_user_config_then_appends_integration() {
        let rc = bash_rcfile();
        assert!(rc.contains("/etc/profile"));
        assert!(rc.contains("~/.bash_profile"));
        assert!(rc.contains("~/.bashrc"));
        assert!(rc.contains("__tty7"));
        assert!(rc.contains("133;"));
    }

    /// Runs `bash_rcfile()` against a throwaway $HOME whose ~/.bashrc appends a
    /// line every time it is sourced, and reports how many lines it left.
    ///
    /// `None` means there is no usable bash on this box, so there is nothing to
    /// assert either way.
    #[cfg(unix)]
    fn bashrc_sourcings(files: &[(&str, &str)]) -> Option<Sourcings> {
        let home = throwaway_dir("tty7-rcfile-home-")?;
        let ticks = home.join("ticks");
        let profile_ticks = home.join("profile-ticks");
        for (name, body) in files {
            // Every startup file the caller supplies leaves its own mark, so a
            // rung of the chain that stops being walked is visible. Counting
            // only `.bashrc` cannot see that: the unconditional fallback at the
            // end sources it whether the profile ran or not, so dropping
            // `.bash_login` from the chain entirely still leaves the count at
            // one — checked by mutating the rung out, and it passed.
            let body = format!("printf 'tick\n' >> '{}'\n{body}", profile_ticks.display());
            std::fs::write(home.join(name), body).expect("write startup file");
        }
        std::fs::write(
            home.join(".bashrc"),
            format!("printf 'tick\\n' >> '{}'\n", ticks.display()),
        )
        .expect("write .bashrc");
        let rcfile = home.join("rcfile");
        std::fs::write(&rcfile, bash_rcfile()).expect("write rcfile");

        let ran = std::process::Command::new("bash")
            .arg("--rcfile")
            .arg(&rcfile)
            .args(["-i", "-c", "true"])
            .env("HOME", &home)
            .output();
        let count = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count()
        };
        let sourced = ran.ok().map(|_| Sourcings {
            bashrc: count(&ticks),
            profile: count(&profile_ticks),
        });
        let _ = std::fs::remove_dir_all(&home);
        sourced
    }

    /// How many times the throwaway `$HOME`'s files were read.
    #[cfg(unix)]
    #[derive(Debug)]
    struct Sourcings {
        bashrc: usize,
        /// Across every startup file the case supplied — a case naming one
        /// profile expects exactly one.
        profile: usize,
    }

    /// The profile chain is first-match-wins, and a ~/.bash_profile that exists
    /// only to forward to ~/.bashrc is the common case — so sourcing ~/.bashrc
    /// after the chain ran it a second time for almost everybody.
    #[cfg(unix)]
    #[test]
    fn a_forwarding_bash_profile_does_not_pull_bashrc_in_twice() {
        let shapes: [(&str, &[(&str, &str)]); 5] = [
            (
                "a guarded `.`",
                &[(
                    ".bash_profile",
                    "if [ -f ~/.bashrc ]; then . ~/.bashrc; fi\n",
                )],
            ),
            (
                "an absolute `source`",
                &[(".bash_profile", "source \"$HOME/.bashrc\"\n")],
            ),
            (
                "a bare `source ~/.bashrc`",
                &[(".bash_profile", "source ~/.bashrc\n")],
            ),
            // The wrapper has to survive being called from inside another
            // sourced file, which is where a dotfile collection that splits
            // itself up puts the forward.
            (
                "a forward one file further in",
                &[
                    (".bash_profile", "source ~/.bash_aliases\n"),
                    (".bash_aliases", "source ~/.bashrc\n"),
                ],
            ),
            // The middle rung of bash's own first-match-wins chain, which the
            // replay has to walk the same way.
            (
                "a forwarding .bash_login",
                &[(".bash_login", ". ~/.bashrc\n")],
            ),
        ];
        for (what, files) in shapes {
            let Some(sourced) = bashrc_sourcings(files) else {
                return;
            };
            assert_eq!(
                sourced.bashrc, 1,
                "~/.bashrc must be sourced exactly once with {what}, got {sourced:?}",
            );
            assert_eq!(
                sourced.profile,
                files.len(),
                "every startup file must be read exactly once with {what}, got {sourced:?}",
            );
        }
    }

    /// The other half of the rule: a profile that never forwards is why the
    /// unconditional source existed in the first place. Dropping it outright
    /// would leave this user with no aliases, prompt or completions at all.
    #[cfg(unix)]
    #[test]
    fn a_profile_that_never_forwards_still_gets_bashrc_once() {
        let shapes: [(&str, &[(&str, &str)]); 4] = [
            (
                "a .bash_profile",
                &[(".bash_profile", "export TTY7_PROFILE=1\n")],
            ),
            (
                "a .bash_login",
                &[(".bash_login", "export TTY7_PROFILE=1\n")],
            ),
            ("a .profile", &[(".profile", "export TTY7_PROFILE=1\n")]),
            // No profile at all — the plain macOS $HOME.
            ("nothing at all", &[]),
        ];
        for (what, files) in shapes {
            let Some(sourced) = bashrc_sourcings(files) else {
                return;
            };
            assert_eq!(
                sourced.bashrc, 1,
                "~/.bashrc must still arrive with {what}, got {sourced:?}",
            );
            assert_eq!(
                sourced.profile,
                files.len(),
                "the profile must be read exactly once with {what}, got {sourced:?}",
            );
        }
    }

    /// Every shell tty7 injects into is named on both front pages.
    ///
    /// A shipped integration nobody knows about is a feature that may as well
    /// not exist: someone choosing a terminal reads the README, not
    /// `shell_integration.rs`. nushell was the case — implemented, tested, and
    /// written up on the shell-integration page, while both READMEs listed
    /// "zsh, bash, fish, PowerShell" and stopped.
    ///
    /// Both languages, because the Chinese README is a translation of the same
    /// table and drifts the same way.
    #[test]
    fn every_shipped_integration_is_named_in_both_readmes() {
        const EN: &str = include_str!("../../../../README.md");
        const ZH: &str = include_str!("../../../../README.zh-CN.md");

        // The name a reader would look for, per script this file ships.
        let shells = [
            ("zsh", ZSH_INTEGRATION),
            ("bash", BASH_INTEGRATION),
            ("fish", FISH_INTEGRATION),
            ("nushell", NUSHELL_INTEGRATION),
            ("PowerShell", POWERSHELL_INTEGRATION),
        ];
        for (name, body) in shells {
            assert!(
                !body.is_empty(),
                "{name}'s integration script is empty, so this list has gone stale"
            );
            assert!(
                EN.contains(name),
                "{name} has an integration script but README.md never names it"
            );
            assert!(
                ZH.contains(name),
                "{name} has an integration script but README.zh-CN.md never names it"
            );
        }
    }

    #[test]
    fn every_integration_guards_install_on_empty_sentinel() {
        for (shell, body) in [
            ("zsh", ZSH_INTEGRATION),
            ("bash", BASH_INTEGRATION),
            ("fish", FISH_INTEGRATION),
        ] {
            assert!(
                body.contains(r#"-z "$TTY7_SHELL_INTEGRATION""#),
                "{shell} integration must guard install on the sentinel being empty \
                 (matching setup()'s empty-string reset), not on its mere definedness",
            );
        }
        assert!(
            !FISH_INTEGRATION.contains("set -q TTY7_SHELL_INTEGRATION"),
            "fish must guard on emptiness (`test -z`), never `set -q`",
        );
    }

    #[test]
    fn d_emitter_is_prepended_ahead_of_user_precmd_hooks() {
        assert!(
            ZSH_INTEGRATION.contains("precmd_functions=(__tty7_precmd_d $precmd_functions)"),
            "zsh must prepend the D emitter (add-zsh-hook can only append)"
        );
        assert!(
            BASH_INTEGRATION
                .contains(r#"precmd_functions=(__tty7_precmd_d "${precmd_functions[@]}")"#),
            "bash must prepend the D emitter"
        );
        for (shell, body) in [
            ("zsh", ZSH_INTEGRATION),
            ("bash", BASH_INTEGRATION),
            ("fish", FISH_INTEGRATION),
        ] {
            assert_eq!(
                body.matches("133;D").count(),
                1,
                "{shell} must emit D from exactly one place"
            );
        }
    }

    #[test]
    fn every_cwd_report_escapes_literal_percent() {
        for (shell, body, escape) in [
            ("zsh", ZSH_INTEGRATION, r"${PWD//\%/%25}"),
            ("bash", BASH_INTEGRATION, r"${PWD//\%/%25}"),
            (
                "fish",
                FISH_INTEGRATION,
                "string replace --all '%' '%25' -- $PWD",
            ),
        ] {
            assert!(
                body.contains(escape),
                "{shell}'s OSC 7 reporter must %-escape the literal percent"
            );
            assert!(
                !body.contains(r#" "$PWD";"#),
                "{shell} must not emit the raw $PWD in its OSC 7 report"
            );
        }
        assert!(
            BASH_INTEGRATION.contains(r"${d//\%/%25}"),
            "bash's msys OSC 7 reporter must %-escape the literal percent too"
        );
    }

    #[test]
    fn bash_reports_a_windows_path_under_msys() {
        let s = BASH_INTEGRATION;
        assert!(
            s.contains(r#"if [[ "$OSTYPE" == msys* || "$OSTYPE" == cygwin* ]]"#),
            "bash must detect msys/cygwin to pick its cwd reporter"
        );
        assert!(
            s.contains("builtin pwd -W"),
            "bash's msys branch must translate the cwd with `pwd -W`"
        );
        assert!(
            s.contains(r#"file://%s/%s"#),
            "bash's msys branch must make the translated path URI-absolute"
        );
        assert!(
            s.contains(r#"[[ "$d" == ?:* ]] || return 0"#),
            "bash's msys branch must report nothing when `pwd -W` yields no drive"
        );
        assert!(
            !s.contains(r#"d="$PWD""#),
            "bash's msys branch must never fall back to the untranslated $PWD"
        );
    }

    #[test]
    fn msys_payload_round_trips_through_parse_osc7() {
        let parse = |payload: &str| {
            crate::daemon::pane::parse_osc7(payload.as_bytes())
                .unwrap_or_else(|| panic!("{payload} should parse"))
        };
        for (translated, want) in [
            ("C:/Users/thoma/repo", "C:/Users/thoma/repo"),
            ("C:/", "C:/"),
            ("D:/work/a b", "D:/work/a b"),
            ("C:/tmp/a%25c", "C:/tmp/a%c"),
        ] {
            let got = parse(&format!("7;file://localhost/{translated}"));
            let want = if cfg!(windows) {
                PathBuf::from(want)
            } else {
                PathBuf::from(format!("/{want}"))
            };
            assert_eq!(got, want, "payload for {translated}");
        }

        if cfg!(windows) {
            let got = parse("7;file://localhost/c/Users/thoma");
            assert_ne!(got, PathBuf::from("C:/Users/thoma"));
            assert!(
                !got.is_absolute(),
                "{got:?} is drive-relative — Windows resolves it against the \
                 current drive, which is why it must never be emitted"
            );
        }
    }

    #[test]
    fn setup_fish_injects_startup_command_without_files() {
        let inj = setup_fish().expect("fish injection is infallible");
        assert_eq!(inj.args[0], "-C");
        assert!(inj.args[1].contains("__tty7"));
        assert!(inj.args[1].contains("133;"));
        assert!(inj.env.is_empty());
        assert!(!inj.replaces_argv);
        assert!(inj.dir.is_none());
    }

    #[test]
    fn setup_zsh_writes_redirectors_and_points_zdotdir_at_them() {
        let inj = setup_zsh().expect("zsh setup should succeed");
        let dir = inj.dir.clone().expect("zsh needs a throwaway dir");
        assert_eq!(
            inj.env.get("ZDOTDIR").map(String::as_str),
            Some(dir.to_string_lossy().as_ref())
        );
        assert!(!inj.replaces_argv);
        assert!(inj.args.is_empty());
        for (name, body) in zsh_redirectors() {
            let written = std::fs::read_to_string(dir.join(name)).expect("redirector written");
            assert_eq!(written, body);
        }
        assert!(is_our_zdotdir(&dir.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_bash_writes_rcfile_and_forces_non_login() {
        let inj = setup_bash().expect("bash setup should succeed");
        let dir = inj.dir.clone().expect("bash needs a throwaway dir");
        assert_eq!(inj.args[0], "--rcfile");
        assert_eq!(inj.args[2], "-i");
        assert!(inj.replaces_argv);
        let rc = std::fs::read_to_string(&inj.args[1]).expect("rcfile written");
        assert_eq!(rc, bash_rcfile());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_dispatches_by_shell_and_sets_sentinel() {
        let inj = setup(Some("zsh"), &[], false).expect("zsh setup");
        assert_eq!(
            inj.env.get("TTY7_SHELL_INTEGRATION").map(String::as_str),
            Some("")
        );
        if let Some(d) = inj.dir {
            let _ = std::fs::remove_dir_all(d);
        }

        assert!(setup(Some("zsh"), &[], true).is_none());

        let inj = setup(Some("fish"), &[], false).expect("fish setup");
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));

        assert!(setup(Some("fish"), &[], true).is_none());

        let bash = if cfg!(windows) {
            "C:/Program Files/Git/bin/bash.exe"
        } else {
            "bash"
        };
        let inj = setup(Some(bash), &[], false).expect("bash setup");
        assert!(inj.replaces_argv);
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));
        if let Some(d) = inj.dir {
            let _ = std::fs::remove_dir_all(d);
        }

        assert!(setup(Some(bash), &[], true).is_none());

        let inj = setup(Some("powershell.exe"), &[], false).expect("powershell setup");
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));
        assert!(inj.dir.is_none());
        assert!(!inj.replaces_argv);

        assert!(setup(Some("pwsh"), &[], true).is_none());

        let inj = setup(Some("nu.exe"), &[], false).expect("nushell setup");
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));
        assert_eq!(inj.args[0], "--config");
        assert!(!inj.replaces_argv);
        if let Some(d) = inj.dir {
            let _ = std::fs::remove_dir_all(d);
        }

        assert!(setup(Some("nu.exe"), &[], true).is_none());

        assert!(setup(Some("/bin/sh"), &[], false).is_none());
    }

    #[test]
    fn setup_powershell_injects_encoded_command_without_files() {
        let inj = setup_powershell().expect("powershell injection is infallible");
        assert_eq!(inj.args[0], "-NoLogo");
        assert_eq!(inj.args[1], "-NoExit");
        assert_eq!(inj.args[2], "-EncodedCommand");
        assert_eq!(inj.args.len(), 4);
        let b64 = &inj.args[3];
        assert!(
            b64.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'='),
            "encoded command must be pure base64"
        );
        assert_eq!(decode_utf16le_base64(b64), POWERSHELL_INTEGRATION);
        assert!(inj.env.is_empty());
        assert!(inj.dir.is_none());
        assert!(!inj.replaces_argv);
    }

    /// Every shipped integration reports the same facts about a command.
    ///
    /// The four OSC 133 marks are what everything above them is built on: `A`
    /// and `B` bracket the prompt, `C` says a command started, `D;<code>` says
    /// it finished and how. `tty7 wait --until free`, the exit code in the tab
    /// strip, the "still running" dot, and every orchestration that reads them
    /// go quiet for a shell that skips one -- and quiet is the whole problem,
    /// because nothing reports a mark that was never sent. `7;file://` is the
    /// same story for the working directory the tree and new tabs follow.
    ///
    /// `D` is checked with its separator: a bare `133;D` means "finished,
    /// code unknown", which is a different and much less useful answer than
    /// the one every one of these scripts actually gives.
    ///
    /// Per-shell tests below go further on the shells that need it; this is
    /// the floor none of them may drop through. Not asserted here: an OSC 0/2
    /// title, which only bash and PowerShell send -- on the others the user's
    /// own prompt owns the title and tty7 does not take it from them.
    #[test]
    fn every_integration_reports_the_prompt_the_command_and_its_exit_code() {
        for (shell, body) in [
            ("zsh", ZSH_INTEGRATION),
            ("bash", BASH_INTEGRATION),
            ("fish", FISH_INTEGRATION),
            ("nushell", NUSHELL_INTEGRATION),
            ("PowerShell", POWERSHELL_INTEGRATION),
        ] {
            for (mark, what) in [
                ("133;A", "the prompt starting"),
                ("133;B", "the prompt ending"),
                ("133;C", "a command starting"),
                ("133;D;", "a command finishing, with its exit code"),
                ("7;file://", "the working directory"),
            ] {
                assert!(
                    body.contains(mark),
                    "{shell}'s integration never sends `{mark}`, so {what} is \
                     invisible in a {shell} pane and nothing says so"
                );
            }
        }
    }

    #[test]
    fn powershell_integration_emits_every_osc_133_mark_and_cwd() {
        let s = POWERSHELL_INTEGRATION;
        assert!(s.contains("]133;A"));
        assert!(s.contains("]133;B"));
        assert!(s.contains("]133;C"));
        assert!(s.contains("]133;D;$code"));
        assert!(s.contains("]7;file://"));
        assert!(s.contains("if (-not $env:TTY7_SHELL_INTEGRATION)"));
        let ok_at = s.find("$ok = $?").expect("captures $?");
        let exit_at = s.find("$lastExit = $LASTEXITCODE").expect("captures exit");
        assert!(ok_at < exit_at, "$? must be read before the exit code");
        assert!(s.contains("$global:__Tty7OrigPrompt = $function:prompt"));
        assert!(s.contains("& $global:__Tty7OrigPrompt"));
        assert!(s.contains(".Replace('%', '%25')"));
    }

    #[test]
    fn powershell_integration_sets_an_osc_title() {
        let s = POWERSHELL_INTEGRATION;
        assert!(s.contains("]0;$($global:__Tty7User)@$($global:__Tty7Host):"));
        assert!(s.contains("$titlePath = '~'"));
        assert!(s.contains("$titlePath = $fsPath.Replace('\\', '/')"));
    }

    /// `$env:USERNAME`, `$env:COMPUTERNAME` and `$env:USERPROFILE` are Windows-only
    /// spellings that are simply empty on macOS and Linux — reaching for any of
    /// them is what titled every Unix pwsh pane `@:` plus a raw absolute path
    /// (#583). The script runs unchanged on all three platforms, so the cheapest
    /// guard is to keep those names out of it entirely.
    #[test]
    fn powershell_integration_reads_identity_platform_neutrally() {
        let s = POWERSHELL_INTEGRATION;
        for windows_only in ["$env:USERNAME", "$env:COMPUTERNAME", "$env:USERPROFILE"] {
            assert!(
                !s.contains(windows_only),
                "{windows_only} is empty on macOS and Linux; use the .NET \
                 equivalent so the one script is right on every platform"
            );
        }
        assert!(s.contains("[Environment]::UserName"));
        assert!(s.contains("[Environment]::MachineName"));
        assert!(s.contains("$global:__Tty7Home = if ($HOME)"));
    }

    /// A bare `StartsWith($home)` also matches the sibling directory whose name
    /// merely begins with the home directory's, so `/Users/annex` used to be
    /// retitled `~ex`. The separator has to be part of the match.
    #[test]
    fn powershell_home_abbreviation_requires_a_path_separator() {
        let s = POWERSHELL_INTEGRATION;
        assert!(s.contains("$titlePath.StartsWith($global:__Tty7Home + '/')"));
        assert!(s.contains("$titlePath -eq $global:__Tty7Home"));
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn powershell_encoded_command_round_trips_utf16le() {
        let script = "Write-Host 'héllo ✓'";
        assert_eq!(
            decode_utf16le_base64(&powershell_encoded_command(script)),
            script
        );
    }

    fn decode_utf16le_base64(b64: &str) -> String {
        fn val(c: u8) -> Option<u32> {
            match c {
                b'A'..=b'Z' => Some((c - b'A') as u32),
                b'a'..=b'z' => Some((c - b'a' + 26) as u32),
                b'0'..=b'9' => Some((c - b'0' + 52) as u32),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        let mut bytes = Vec::new();
        let mut acc = 0u32;
        let mut nbits = 0;
        for c in b64.bytes() {
            let Some(v) = val(c) else { continue };
            acc = (acc << 6) | v;
            nbits += 6;
            if nbits >= 8 {
                nbits -= 8;
                bytes.push((acc >> nbits) as u8);
            }
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16(&units).expect("valid UTF-16LE")
    }

    /// A prefix a shell can take is a prefix the sweep recognises.
    ///
    /// This is the check that was missing: zsh's directories were swept for a
    /// while and bash's, nushell's and WSL's were not, because the sweep knew
    /// one prefix and `throwaway_dir` handed out four. Adding a const here
    /// without adding it to the list fails this.
    /// Every shell tty7 injects into is on the page that lists them.
    ///
    /// Nushell was not: it has had a `config.nu` wrapper for as long as the
    /// others have had theirs, and the page's "Which shells" table left it
    /// out, so a nushell user reading it concluded they got nothing.
    ///
    /// The list comes from the dispatch rather than from here, so a sixth
    /// shell has to be written down before this passes.
    #[test]
    fn the_shell_integration_page_lists_every_shell_that_gets_one() {
        const SOURCE: &str = include_str!("shell_integration.rs");
        const PAGE: &str = include_str!("../../../../docs/reference/shell-integration.mdx");

        let injected: Vec<&str> = SOURCE
            .match_indices("ShellKind::")
            .filter_map(|(at, marker)| {
                let rest = &SOURCE[at + marker.len()..];
                let (name, tail) = rest.split_once(" => ")?;
                // Only the arms that actually build an injection; `Wsl => None`
                // on unix is the same shell as the Windows arm above it.
                tail.starts_with("setup_").then_some(name)
            })
            .collect();

        assert!(
            injected.len() >= 5,
            "only {injected:?} were read out of the dispatch"
        );
        let missing: Vec<&&str> = injected
            .iter()
            .filter(|shell| !PAGE.to_lowercase().contains(&shell.to_lowercase()))
            .collect();
        assert!(
            missing.is_empty(),
            "these shells are injected into but not on the page: {missing:?}"
        );
    }

    #[test]
    fn every_prefix_a_shell_can_take_is_one_the_sweep_knows() {
        // Counted from the source rather than listed here. Listing them is
        // what this test used to do, and it could only ever confirm that four
        // constants were among those same four — a fifth shell handed a
        // throwaway directory would have been swept up by nobody, which is
        // how bash's 607 of them were stranded in the first place.
        const SOURCE: &str = include_str!("shell_integration.rs");
        let production = SOURCE.split("mod tests").next().unwrap_or(SOURCE);
        let handed_out: std::collections::BTreeSet<&str> = production
            .match_indices("throwaway_dir(")
            .filter_map(|(at, marker)| {
                let rest = &production[at + marker.len()..];
                let name = rest.split(')').next()?;
                // The definition takes `prefix: &str`; a call names a constant.
                name.starts_with(|c: char| c.is_ascii_uppercase())
                    .then_some(name)
            })
            .collect();
        assert_eq!(
            handed_out.len(),
            THROWAWAY_PREFIXES.len(),
            "these prefixes reach a shell: {handed_out:?}, but the sweep knows \
             {} of them",
            THROWAWAY_PREFIXES.len()
        );

        for prefix in [ZDOTDIR_PREFIX, NU_PREFIX, BASHRC_PREFIX, WSLRC_PREFIX] {
            assert!(
                THROWAWAY_PREFIXES.contains(&prefix),
                "{prefix} is handed to a shell but never swept"
            );
            assert_eq!(
                throwaway_owner(&format!("{prefix}4242-0")),
                Some(4242),
                "{prefix} names its owner in the shape the sweep reads"
            );
        }
    }

    /// Only our own directories, and only ones with a pid in them.
    ///
    /// The sweep deletes, so what it declines to match matters more than what
    /// it matches. `tty7-zdotdir-wsl` is a real name this module writes, and a
    /// pid is the one thing that makes a directory safe to judge.
    /// The daemon still calls the sweep on startup.
    ///
    /// The three tests below hold what [`sweep_dead_zdotdirs`] does; none of
    /// them holds that anyone runs it, and deleting the call from the daemon's
    /// startup left the whole suite green. That is how the directories piled
    /// up in the first place — 4,457 of them — so the call is worth a guard of
    /// its own.
    ///
    /// It reads the source because the only caller binds a listener and then
    /// serves forever, which no unit test can enter. So this proves the call
    /// is written, not that it is reached: a sweep moved somewhere that never
    /// runs would still pass. It catches deletion, which is the way it broke.
    #[test]
    fn the_daemon_startup_still_calls_the_sweep() {
        const STARTUP: &str = include_str!("server.rs");
        assert!(
            STARTUP.contains("shell_integration::sweep_dead_zdotdirs()"),
            "nothing in the daemon calls the sweep any more"
        );
    }

    #[test]
    fn only_a_named_pid_makes_a_directory_ours_to_sweep() {
        // Every shell that takes a scratch directory, not just zsh: bash's had
        // outnumbered zsh's six to one while only zsh was being swept.
        assert_eq!(throwaway_owner("tty7-zdotdir-4242-0"), Some(4242));
        assert_eq!(throwaway_owner("tty7-bashrc-4242-0"), Some(4242));
        assert_eq!(throwaway_owner("tty7-nu-4242-1"), Some(4242));
        assert_eq!(throwaway_owner("tty7-wslrc-9-0"), Some(9));
        assert_eq!(throwaway_owner("tty7-zdotdir-1-17"), Some(1));
        assert_eq!(throwaway_owner("tty7-zdotdir-wsl"), None);
        assert_eq!(throwaway_owner("tty7-zdotdir-4242"), None, "no seq, no pid");
        assert_eq!(
            throwaway_owner("tty7-covtest-4242-0"),
            None,
            "another prefix"
        );
        assert_eq!(throwaway_owner("something-else"), None);
        assert_eq!(throwaway_owner(".hidden"), None);
    }

    /// A live owner's directory is left where it is.
    #[test]
    fn the_sweep_spares_a_living_daemon() {
        let mine =
            std::env::temp_dir().join(format!("{ZDOTDIR_PREFIX}{}-sweeptest", std::process::id()));
        let init = std::env::temp_dir().join(format!("{ZDOTDIR_PREFIX}1-sweeptest"));
        std::fs::create_dir_all(&mine).expect("one owned by this very process");
        std::fs::create_dir_all(&init).expect("one owned by pid 1, which outlives everything");

        sweep_dead_zdotdirs();

        let (kept_mine, kept_init) = (mine.is_dir(), init.is_dir());
        std::fs::remove_dir_all(&mine).ok();
        std::fs::remove_dir_all(&init).ok();
        assert!(kept_mine, "the sweep took the running process's own");
        assert!(kept_init, "the sweep took a live owner's");
    }

    #[test]
    fn throwaway_dir_is_unique_per_call() {
        let a = throwaway_dir("tty7-test-").expect("dir a");
        let b = throwaway_dir("tty7-test-").expect("dir b");
        assert_ne!(a, b);
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tty7-test-")
        );
        assert!(a.is_dir() && b.is_dir());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn setup_nushell_writes_config_and_points_at_it() {
        let inj = setup_nushell().expect("nushell setup should succeed");
        let dir = inj.dir.clone().expect("nushell needs a throwaway dir");
        assert_eq!(inj.args[0], "--config");
        assert_eq!(inj.args.len(), 2);
        let written = std::fs::read_to_string(&inj.args[1]).expect("config written");
        assert_eq!(
            written,
            nushell_config_script_with(nushell_user_config_path().as_deref())
        );
        assert!(inj.env.is_empty());
        assert!(!inj.replaces_argv);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nushell_integration_emits_every_osc_133_mark_and_cwd() {
        let s = NUSHELL_INTEGRATION;
        for mark in ["133;A", "133;B", "133;C", "133;D;"] {
            assert!(s.contains(mark), "Nushell must emit {mark:?}");
        }
        assert!(s.contains("7;file://"));
        assert!(
            s.contains(r#"($nu.os-info.name? | default '') == 'windows'"#),
            "the backslash translation must be gated on Windows — a Unix path \
             may legally contain a literal backslash"
        );
        assert!(
            s.contains(r#"str replace -a '%' '%25'"#),
            "must percent-escape"
        );
        assert!(s.contains("__tty7_cmd_active"));
        assert!(s.contains("hide-env __tty7_cmd_active"));
        assert!(s.contains("prompt_indicator?"));
        assert!(
            s.contains(r##"($env.TTY7_SHELL_INTEGRATION? | default "") == """##),
            "must guard install on the sentinel being empty, like the other shells"
        );
    }

    #[test]
    fn nushell_config_script_substitutes_a_literal_source_or_a_noop() {
        let with = nushell_config_script_with(Some(Path::new(
            r"C:\Users\ann\AppData\Roaming\nushell\config.nu",
        )));
        assert!(with.contains("source 'C:\\Users\\ann\\AppData\\Roaming\\nushell\\config.nu'"));
        let source_at = with.find("source ").expect("sources the user config");
        let hooks_at = with.find("hooks.pre_prompt").expect("appends hooks");
        assert!(
            source_at < hooks_at,
            "the user config must be sourced before the hooks are appended"
        );

        let without = nushell_config_script_with(None);
        assert!(
            !without.contains("source '") && !without.contains("source \""),
            "no config.nu means nothing to source"
        );
        assert!(without.contains("no user config.nu to restore"));
        assert!(without.contains("hooks.pre_prompt"));
    }

    #[test]
    fn nu_string_literal_quotes_paths_for_the_wrapper() {
        assert_eq!(
            nu_string_literal(r"C:\Users\ann\nushell\config.nu"),
            r"'C:\Users\ann\nushell\config.nu'"
        );
        assert_eq!(
            nu_string_literal("/home/ann/.config/nushell/config.nu"),
            "'/home/ann/.config/nushell/config.nu'"
        );
        // An apostrophe (legal on Windows) forces the double-quoted fallback.
        assert_eq!(
            nu_string_literal(r"C:\it's\config.nu"),
            r#""C:\\it's\\config.nu""#
        );
        // Single quotes are fully literal in Nushell, so `$` needs no escaping
        // there; only an apostrophe forces the double-quoted fallback.
        assert_eq!(
            nu_string_literal(r"C:\a$b\config.nu"),
            r"'C:\a$b\config.nu'"
        );
        assert_eq!(
            nu_string_literal(r"C:\it's\$x\config.nu"),
            r#""C:\\it's\\\$x\\config.nu""#
        );
    }

    /// nu-path resolves the config dir from `$XDG_CONFIG_HOME` on every
    /// platform, but only when it is non-empty *and* absolute. Both halves are
    /// load-bearing: the original Windows arm ignored the variable outright,
    /// and the original non-Windows arm accepted a relative value nu rejects.
    #[test]
    fn nushell_config_dir_follows_nu_s_own_resolution_rules() {
        use std::ffi::OsStr;
        // `/x` is not absolute on Windows (no prefix), so the fixtures have to
        // be shaped for the host or the rule under test is never exercised.
        let abs_xdg = if cfg!(windows) { r"C:\xdg" } else { "/xdg" };
        let platform = || {
            Some(PathBuf::from(if cfg!(windows) {
                r"C:\platform"
            } else {
                "/platform"
            }))
        };
        let want = |base: &str| Some(PathBuf::from(base).join("nushell"));

        // Unset, empty, or relative: nu ignores it and takes the platform dir.
        for ignored in [None, Some(OsStr::new("")), Some(OsStr::new("relative/dir"))] {
            assert_eq!(
                nushell_config_dir_from(ignored, platform()),
                want(&platform().unwrap().to_string_lossy()),
                "XDG_CONFIG_HOME {ignored:?} must not displace the platform dir"
            );
        }

        // Non-empty and absolute: it wins, even where there is no platform dir.
        assert_eq!(
            nushell_config_dir_from(Some(OsStr::new(abs_xdg)), platform()),
            want(abs_xdg)
        );
        assert_eq!(
            nushell_config_dir_from(Some(OsStr::new(abs_xdg)), None),
            want(abs_xdg)
        );
        assert_eq!(nushell_config_dir_from(None, None), None);
    }

    /// The macOS half of the same rule, stated as itself: nu follows the
    /// platform convention there, which is *not* where tty7 keeps its own
    /// config (`~/.config/tty7`). Reading `~/.config/nushell` on a Mac finds
    /// nothing, and the wrapper then replaces a config it thinks is absent.
    #[cfg(target_os = "macos")]
    #[test]
    fn nushell_config_dir_is_application_support_on_macos() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        assert_eq!(
            platform_config_dir(),
            Some(home.join("Library/Application Support"))
        );
    }

    /// Ask the real binary. A hand-copy of nu's resolution rules is only as
    /// good as the reading behind it — this is the check that fails outright
    /// if the two ever disagree on this machine.
    #[cfg(unix)]
    #[test]
    fn nushell_config_dir_is_what_nu_itself_reports() {
        let Some(nu) = test_nushell_path() else {
            eprintln!("skipping: Nushell not installed");
            return;
        };
        let xdg_absolute = std::env::temp_dir().join("tty7-nu-xdg");
        let cases: [Option<&std::ffi::OsStr>; 4] = [
            None,
            Some(std::ffi::OsStr::new("")),
            Some(std::ffi::OsStr::new("relative/dir")),
            Some(xdg_absolute.as_os_str()),
        ];
        for xdg in cases {
            let mut cmd = std::process::Command::new(&nu);
            cmd.args(["-n", "-c", "$nu.default-config-dir"]);
            match xdg {
                Some(v) => cmd.env("XDG_CONFIG_HOME", v),
                None => cmd.env_remove("XDG_CONFIG_HOME"),
            };
            let out = cmd.output().expect("run nu");
            // A relative value makes nu warn before it answers, so take the
            // last line, not the whole of stdout.
            let stdout = String::from_utf8_lossy(&out.stdout);
            let theirs = PathBuf::from(stdout.lines().last().unwrap_or_default().trim());
            let ours = nushell_config_dir_from(xdg, platform_config_dir())
                .expect("this machine has a config dir");
            assert_eq!(
                ours, theirs,
                "tty7 and nu disagree on the config dir for XDG_CONFIG_HOME={xdg:?}"
            );
        }
    }

    /// End to end over a real pty: a user who *has* a config.nu must still
    /// have it after tty7 points `nu --config` at the wrapper. `--config`
    /// replaces the user's config rather than adding to it, so a wrapper that
    /// fails to source it silently strips their prompt, aliases and
    /// keybindings.
    #[cfg(unix)]
    #[test]
    fn nushell_restores_the_user_config_and_still_reports_the_prompt_cycle() {
        let Some(nu) = test_nushell_path() else {
            eprintln!("skipping: Nushell not installed");
            return;
        };
        let nu = nu.to_string_lossy().into_owned();
        let home = tempfile::tempdir().expect("tempdir");
        let user_config = home.path().join("config.nu");
        // Shaped like a real config.nu: a wholesale `$env.config = {...}` (the
        // conventional ending, and the one that would wipe hooks added before
        // it), plus a marker only the user's own file could have set.
        std::fs::write(
            &user_config,
            "$env.config = { show_banner: false }\n$env.TTY7_USER_CONFIG_LOADED = \"yes\"\n",
        )
        .expect("write user config");

        let mut injection = setup_nushell_with(Some(&user_config)).expect("nushell integration");
        // What `setup` does at every spawn boundary, and what this test
        // bypasses by reaching for `setup_nushell_with` directly. Without it
        // the wrapper is a no-op whenever the test itself runs inside an
        // integrated pane — the guard sees the inherited `1` and skips the
        // whole block, and the marks in the transcript are then Nushell's own.
        injection
            .env
            .insert("TTY7_SHELL_INTEGRATION".to_string(), String::new());
        let text = prompt_cycle_over_pty(
            &nu,
            &injection,
            // One line: Nushell's line editor holds the tty in raw mode and
            // drops what arrives while a command is running, so a second `\r`
            // never reaches it and its prompt cycle never happens. `1 / 0` is
            // the failure — a bare `false` in Nushell is a value.
            b"print $\"probe=($env.TTY7_USER_CONFIG_LOADED? | default 'MISSING')\"; 1 / 0\r",
            Some(home.path()),
        );

        assert!(
            text.contains("probe=yes"),
            "the user's own config.nu must survive --config; got:\n{text}"
        );
        // Assert on tty7's *own* marks, not on the substring `133;A`: stock
        // Nushell emits its own OSC 133 cycle alongside these, so a bare
        // substring passes even when the wrapper never ran. tty7's hooks
        // terminate with BEL and Nushell's with ST, which tells them apart.
        for mark in ["133;A\u{7}", "133;C\u{7}", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "Nushell must still report {mark:?} after sourcing the user config; got:\n{text}"
            );
        }
        // Same reasoning for the cwd: the hook falls back to `localhost` for
        // the host, while Nushell's native OSC 7 names the real one.
        assert!(
            text.contains("7;file://localhost/"),
            "tty7's own OSC 7 must be among the reports; got:\n{text}"
        );
        assert_eq!(
            last_osc7(&text),
            home.path().canonicalize().expect("canonical tempdir"),
            "the pane's reported cwd must be where nu actually is"
        );
    }

    /// `shells::nushell_path` is Windows-only (nu has no fixed install path
    /// there); on unix the binary is whatever `nu` resolves to on PATH.
    #[cfg(unix)]
    fn test_nushell_path() -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("nu"))
            .find(|p| p.is_file())
    }

    #[cfg(windows)]
    #[test]
    fn nushell_reports_the_full_prompt_cycle_over_a_real_pty() {
        let Some(nu) = crate::core::shells::nushell_path() else {
            eprintln!("skipping: Nushell not installed");
            return;
        };
        let nu = nu.to_string_lossy().into_owned();
        let injection = setup(Some(&nu), &[], false).expect("nushell integration");
        // One line: Nushell's line editor holds the tty in raw mode and can
        // drop what arrives while a command is running, so a second `\r` may
        // never reach it. `cd` first, then a command that fails — `1 / 0`
        // errors with exit status 1 (a literal `false` is just a value).
        let text = prompt_cycle_over_pty(
            &nu,
            &injection,
            b"cd C:/Windows; 1 / 0\r",
            Some(Path::new("C:/")),
        );

        for mark in ["133;A", "133;B", "133;C", FAILED_COMMAND_MARK] {
            assert!(
                text.contains(mark),
                "Nushell must report {mark:?}; got:\n{text}"
            );
        }
        let cwd = reported_cwd(&text);
        assert!(
            cwd.exists(),
            "Nushell reported a cwd the Windows side cannot resolve: {cwd:?} \
             — a drive-relative path, so the URI leading-slash translation regressed"
        );
        assert_eq!(
            last_osc7(&text),
            PathBuf::from("C:/Windows"),
            "`cd` must move the pane's reported cwd; got:\n{text}"
        );
    }
}
