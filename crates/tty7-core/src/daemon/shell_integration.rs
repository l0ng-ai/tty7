use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ZSH_INTEGRATION: &str = r#"
# --- tty7 shell integration (zsh) ---
if [[ -o interactive ]] && [[ -z "$TTY7_SHELL_INTEGRATION" ]]; then
  export TTY7_SHELL_INTEGRATION=1

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
      Write-Host -NoNewline "$($global:__Tty7Esc)]7;file://$($env:COMPUTERNAME)$p$($global:__Tty7Bel)"

      # OSC 0 window/tab title "user@host:dir". PowerShell profiles don't set a
      # title the way macOS's default zsh does, so without this every tty7 tab on
      # Windows stays generic. Forward slashes (so tty7's tab-label parser can take
      # the last path segment) and home shown as `~`. Re-emitted each prompt so it
      # tracks cwd; a full-screen app's own title still overrides it while it runs.
      $titlePath = $fsPath.Replace('\', '/')
      if ($env:USERPROFILE) {
        $userHome = $env:USERPROFILE.Replace('\', '/')
        if ($titlePath.StartsWith($userHome)) {
          $titlePath = '~' + $titlePath.Substring($userHome.Length)
        }
      }
      Write-Host -NoNewline "$($global:__Tty7Esc)]0;$($env:USERNAME)@$($env:COMPUTERNAME):$titlePath$($global:__Tty7Bel)"
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

pub struct Injection {
    pub env: HashMap<String, String>,
    pub args: Vec<String>,
    pub replaces_argv: bool,
    pub dir: Option<PathBuf>,
}

const ZDOTDIR_PREFIX: &str = "tty7-zdotdir-";

fn is_our_zdotdir(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(ZDOTDIR_PREFIX))
}

fn real_user_zdotdir() -> Option<String> {
    if let Ok(z) = std::env::var("TTY7_USER_ZDOTDIR") {
        if !z.is_empty() {
            return Some(z);
        }
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

fn throwaway_dir(prefix: &str) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut dir = std::env::temp_dir();
    dir.push(format!("{prefix}{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
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
    let dir = throwaway_dir("tty7-bashrc-")?;
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

/// POSIX single-quoting, for a body some other shell has to re-parse.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The bootstrap `sh` runs inside the distro. `$SHELL` is the only place the
/// user's real shell is named, so every arm dispatches on it: bash re-execs
/// through the rcfile written on the Windows side, fish carries its integration
/// inline the way `remote::bootstrap_command` does over SSH, and anything else
/// falls back to a plain login shell.
///
/// `sh` parses this script, so the fish body is POSIX-quoted here — unlike the
/// SSH path, where the far end's own login shell parses the bootstrap and
/// `remote::fish_bootstrap` has to quote it the way fish reads quotes.
#[cfg_attr(not(windows), allow(dead_code))]
fn wsl_exec_script() -> String {
    format!(
        concat!(
            r#"case "${{SHELL:-}}" in "#,
            r#"*/bash) exec "$SHELL" --rcfile "$TTY7_RC" -i ;; "#,
            r#"*/fish) exec "$SHELL" -C {} -l ;; "#,
            r#"*) exec "${{SHELL:-/bin/sh}}" -l ;; "#,
            "esac"
        ),
        shell_quote(FISH_INTEGRATION)
    )
}

#[cfg(windows)]
fn setup_wsl(args: &[String]) -> Option<Injection> {
    let distro = wsl_distro(args);
    let dir = throwaway_dir("tty7-wslrc-")?;
    let rcfile = dir.join("bashrc");
    std::fs::write(&rcfile, bash_rcfile()).ok()?;

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

    let mut env = HashMap::new();
    env.insert(
        WSL_RCFILE_ENV.to_string(),
        rcfile.to_string_lossy().into_owned(),
    );
    env.insert(
        "WSLENV".to_string(),
        wslenv_with(
            std::env::var("WSLENV").ok().as_deref(),
            &[&format!("{WSL_RCFILE_ENV}/p")],
        ),
    );

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
    let mut injection = match shell_kind(program)? {
        ShellKind::Zsh => setup_zsh(),
        ShellKind::Fish => setup_fish(),
        ShellKind::Bash if !has_custom_args => setup_bash(),
        ShellKind::Bash => None,
        ShellKind::PowerShell if !has_custom_args => setup_powershell(),
        ShellKind::PowerShell => None,
        #[cfg(windows)]
        ShellKind::Wsl if !has_custom_args => setup_wsl(args),
        ShellKind::Wsl => None,
    }?;

    injection
        .env
        .insert("TTY7_SHELL_INTEGRATION".to_string(), String::new());

    Some(injection)
}

pub mod remote {
    use super::{FISH_INTEGRATION, bash_rcfile, shell_quote, zsh_redirectors};

    pub const PROBE_COMMAND: &str = "echo __tty7_shell; echo $SHELL";

    const PROBE_MARKER: &str = "__tty7_shell";

    const HEREDOC: &str = "__TTY7_RC_EOF__";

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum RemoteShell {
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

    pub fn parse_probe(output: &str) -> Option<(RemoteShell, String)> {
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

    pub fn bootstrap_command(shell: RemoteShell, shell_path: &str) -> String {
        match shell {
            RemoteShell::Zsh => zsh_bootstrap(shell_path),
            RemoteShell::Bash => bash_bootstrap(shell_path),
            RemoteShell::Fish => fish_bootstrap(shell_path),
        }
    }

    fn fish_quote(s: &str) -> String {
        format!("'{}'", s.replace('\\', r"\\").replace('\'', r"\'"))
    }

    fn write_file(out: &mut String, name: &str, body: &str) {
        out.push_str(&format!(
            "command cat > \"$__tty7_d/{name}\" <<'{HEREDOC}'\n{}\n{HEREDOC}\n",
            body.trim_end_matches('\n')
        ));
    }

    fn zsh_bootstrap(shell_path: &str) -> String {
        let mut out = String::new();
        out.push_str("__tty7_d=${TMPDIR:-/tmp}/tty7-zdotdir-$$\n");
        out.push_str("command mkdir -p \"$__tty7_d\" 2>/dev/null\n");

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
        out.push_str(&format!("exec {} -l\n", shell_quote(shell_path)));
        out
    }

    fn bash_bootstrap(shell_path: &str) -> String {
        let quoted = shell_quote(shell_path);
        let mut out = String::new();
        out.push_str("__tty7_d=${TMPDIR:-/tmp}/tty7-bashrc-$$\n");
        out.push_str("command mkdir -p \"$__tty7_d\" 2>/dev/null\n");
        write_file(
            &mut out,
            "bashrc",
            &format!("{}{BASH_CLEANUP_HOOK}", bash_rcfile()),
        );
        out.push_str("if [ -s \"$__tty7_d/bashrc\" ]; then\n");
        out.push_str("export TTY7_RM_DIR=\"$__tty7_d\"\n");
        out.push_str(&format!("exec {quoted} --rcfile \"$__tty7_d/bashrc\" -i\n"));
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
    #[cfg(windows)]
    const FAILED_COMMAND_MARK: &str = "133;D;1\u{7}";

    #[cfg(windows)]
    fn prompt_cycle_over_pty(program: &str, injection: &Injection) -> String {
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
        let mut child = pty.slave.spawn_command(cmd).expect("spawn shell");

        let mut writer = pty.master.take_writer().expect("writer");
        let mut reader = pty.master.try_clone_reader().expect("reader");
        writer.write_all(b"false\n").expect("write");
        writer.flush().expect("flush");

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
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // Stop only once the typed command's own D report lands. A shell can
            // emit an unpaired D while drawing its first prompt, and breaking on
            // that would sample the transcript before the prompt cycle under test
            // has run at all.
            if String::from_utf8_lossy(&out).contains(FAILED_COMMAND_MARK) {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        drop(pty.master);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[cfg(windows)]
    fn reported_cwd(text: &str) -> PathBuf {
        let payload = text
            .split("\u{1b}]")
            .find(|s| s.starts_with("7;file://"))
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
        let text = prompt_cycle_over_pty(&bash, &injection);

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
        let text = prompt_cycle_over_pty("wsl.exe", &injection);

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

        assert!(script.contains(r#"*/bash) exec "$SHELL" --rcfile "$TTY7_RC" -i ;;"#));
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
    fn bashrc_sourcings(profile: Option<(&str, &str)>) -> Option<usize> {
        let home = throwaway_dir("tty7-rcfile-home-")?;
        let ticks = home.join("ticks");
        if let Some((name, body)) = profile {
            std::fs::write(home.join(name), body).expect("write profile");
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
        let sourced = ran.ok().map(|_| {
            std::fs::read_to_string(&ticks)
                .unwrap_or_default()
                .lines()
                .count()
        });
        let _ = std::fs::remove_dir_all(&home);
        sourced
    }

    /// The profile chain is first-match-wins, and a ~/.bash_profile that exists
    /// only to forward to ~/.bashrc is the common case — so sourcing ~/.bashrc
    /// after the chain ran it a second time for almost everybody.
    #[cfg(unix)]
    #[test]
    fn a_forwarding_bash_profile_does_not_pull_bashrc_in_twice() {
        for forward in [
            "if [ -f ~/.bashrc ]; then . ~/.bashrc; fi\n",
            "source \"$HOME/.bashrc\"\n",
        ] {
            let Some(sourced) = bashrc_sourcings(Some((".bash_profile", forward))) else {
                return;
            };
            assert_eq!(
                sourced, 1,
                "~/.bashrc must be sourced exactly once for {forward:?}, got {sourced}",
            );
        }
    }

    /// The other half of the rule: a profile that never forwards is why the
    /// unconditional source existed in the first place. Dropping it outright
    /// would leave this user with no aliases, prompt or completions at all.
    #[cfg(unix)]
    #[test]
    fn a_profile_that_never_forwards_still_gets_bashrc_once() {
        for profile in [
            Some((".bash_profile", "export TTY7_PROFILE=1\n")),
            Some((".profile", "export TTY7_PROFILE=1\n")),
            // No profile at all — the plain macOS $HOME.
            None,
        ] {
            let Some(sourced) = bashrc_sourcings(profile) else {
                return;
            };
            assert_eq!(
                sourced, 1,
                "~/.bashrc must still arrive for {profile:?}, got {sourced}",
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

        let inj = setup(Some("fish"), &[], false).expect("fish setup");
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));

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
        assert!(s.contains("]0;$($env:USERNAME)@$($env:COMPUTERNAME):"));
        assert!(s.contains("$titlePath = '~'"));
        assert!(s.contains("$titlePath = $fsPath.Replace('\\', '/')"));
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
}
