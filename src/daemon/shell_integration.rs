//! Shell integration: inject a small startup snippet into the shell tty7 spawns
//! so the shell *actively reports* its state — prompt boundaries, command
//! start/finish, exit codes, and cwd — instead of us guessing from the outside.
//!
//! This is the foundation the inline input editor builds on. The reporting
//! protocol is the FinalTerm / iTerm2 **OSC 133** semantic-prompt standard, so it
//! interoperates with the wider ecosystem rather than a bespoke scheme:
//!   - `OSC 133 ; A ST`            prompt start
//!   - `OSC 133 ; B ST`            prompt end / command input begins
//!   - `OSC 133 ; C ST`            command output begins (command executing)
//!   - `OSC 133 ; D ; <exit> ST`   command finished, with its exit code
//! plus `OSC 7` to report the cwd precisely (many login shells don't emit it
//! unless they think they're in Terminal.app).
//!
//! Supports zsh, bash, fish and PowerShell; each needs a different injection
//! mechanism because the shells disagree on how much control they hand an
//! integrator:
//!   - **zsh** has `ZDOTDIR`, an env var that retargets *all* of its startup
//!     files at once — the cleanest hook of the four. See [`zsh_redirectors`].
//!   - **fish** has no such redirect, but its `-C`/`--init-command` flag runs
//!     extra commands after fish's own (unmodified) config load — no throwaway
//!     directory needed at all. See [`setup_fish`].
//!   - **bash** has neither: no env var retargets its rc file, and `--rcfile`
//!     (the only override it does have) is silently ignored for *login*
//!     shells, which is how terminals normally spawn it. So we spawn bash as a
//!     plain non-login shell instead and have our rcfile manually replay the
//!     login-shell startup-file chain (`/etc/profile`, `~/.bash_profile` &
//!     co.) before layering hooks on top — see [`setup_bash`] and
//!     [`Injection::force_non_login`]. Bash also has no native precmd/preexec,
//!     so the hook body vendors the relevant parts of
//!     [bash-preexec](https://github.com/rcaloras/bash-preexec) (MIT), the
//!     same shim VS Code relies on for this.
//!   - **PowerShell** (the Windows default, and any `pwsh`) has no dotfile
//!     redirect either, but `-EncodedCommand` runs a script *after* its own
//!     profiles load — like fish's `-C`, no file on disk. It has no
//!     precmd/preexec, so — following Warp and VS Code — the body wraps two
//!     host hooks: the `prompt` function (for the A/B/D marks + cwd) and
//!     `PSConsoleHostReadLine`, PSReadLine's line reader (the closest thing to
//!     a preexec, for the C mark). See [`setup_powershell`].
//!
//! Across all four: **the user's own dotfiles are never modified** — the
//! mechanisms above only affect shells tty7 itself launches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The zsh integration body, sourced from our injected `.zshrc` after the user's
/// own `.zshrc` has run. Guarded so it installs exactly once per interactive
/// shell. See the module docs for the OSC 133 semantics.
const ZSH_INTEGRATION: &str = r#"
# --- tty7 shell integration (zsh) ---
if [[ -o interactive ]] && [[ -z "$TTY7_SHELL_INTEGRATION" ]]; then
  export TTY7_SHELL_INTEGRATION=1

  __tty7_osc() { builtin printf '\e]%s\a' "$1"; }

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
  # prompt (no command yet) doesn't emit a bogus D.
  __tty7_preexec() {
    __tty7_cmd_active=1
    __tty7_osc "133;C"
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

/// The fish integration body, passed verbatim as a `-C`/`--init-command`
/// argument (see [`setup_fish`]) — fish has already loaded the user's *real*
/// `config.fish` by the time this runs, so unlike zsh/bash there's nothing here
/// to source manually.
///
/// fish has no event that fires *after* the prompt is drawn, so the B marker
/// (prompt end / input begins) can't be emitted from an `--on-event` handler
/// the way A/C/D are — it has to be spliced into `fish_prompt` itself. We
/// capture whatever `fish_prompt` already is (the user's own, or a prompt
/// framework's) and wrap it: call the original, then emit B right after.
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

  # The daemon percent-decodes the OSC 7 payload; escape literal `%` as %25 so
  # a path like /tmp/a%20b round-trips instead of decoding to /tmp/a b.
  function __tty7_report_cwd
    printf '\e]7;file://%s%s\a' (hostname) (string replace --all '%' '%25' -- $PWD)
  end

  function __tty7_preexec --on-event fish_preexec
    set -g __tty7_cmd_active 1
    __tty7_osc "133;C"
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

/// The bash integration body, appended after the replayed login-file chain
/// (see [`setup_bash`]). Bash has no native precmd/preexec, so this vendors the
/// core mechanism from [bash-preexec](https://github.com/rcaloras/bash-preexec)
/// (MIT) — the same shim VS Code uses — trimmed of everything but the
/// precmd/preexec plumbing: a `DEBUG` trap infers "a command is genuinely about
/// to run interactively" (as opposed to firing mid-completion, mid readline
/// binding, or for a piece of `PROMPT_COMMAND` itself), and `PROMPT_COMMAND`
/// runs registered precmd functions before each prompt.
///
/// If the user's own `.bashrc` already loaded bash-preexec (several prompt
/// frameworks bundle it) we don't install it a second time — re-running the
/// install sequence would clear and never restore the already-installed
/// `DEBUG` trap. We detect that via bash-preexec's own `bash_preexec_imported`
/// sentinel and, either way, register our hooks through its public extension
/// points (`precmd_functions` / `preexec_functions`) rather than the "function
/// literally named `precmd`/`preexec`" convenience, which could collide with
/// the user's own.
const BASH_INTEGRATION: &str = r#"
# --- tty7 shell integration (bash) ---
if [[ $- == *i* ]] && [[ -z "$TTY7_SHELL_INTEGRATION" ]]; then
  export TTY7_SHELL_INTEGRATION=1

  __tty7_osc() { builtin printf '\e]%s\a' "$1"; }
  # Escape literal `%` as %25 — the daemon percent-decodes the OSC 7 payload.
  __tty7_report_cwd() { builtin printf '\e]7;file://%s%s\a' "${HOSTNAME:-localhost}" "${PWD//\%/%25}"; }

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

  __tty7_preexec() {
    __tty7_cmd_active=1
    __tty7_osc "133;C"
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

/// The PowerShell integration body, base64-encoded (see
/// [`powershell_encoded_command`]) and passed as `-EncodedCommand`, which
/// PowerShell runs *after* loading the user's profiles — so, like fish's `-C`,
/// it layers hooks on top of the user's own prompt without a file on disk and
/// without touching their config.
///
/// PowerShell has no precmd/preexec, so — mirroring Warp and VS Code — we wrap
/// two host hooks:
///   - **`prompt`** runs before each prompt is drawn. It emits `133;D` (the
///     last command's exit code) and the `OSC 7` cwd as side effects, then
///     returns the user's own prompt wrapped in `133;A` … `133;B`. The byte
///     order is therefore `[D][cwd][A]prompt[B]`, exactly what the daemon's
///     sniffer keys `at_prompt` off (see `daemon::pane::handle_osc133`).
///   - **`PSConsoleHostReadLine`** is PSReadLine's line reader — the closest
///     thing PowerShell has to a preexec. After it returns the submitted line,
///     before the command runs, we emit `133;C` (command output begins).
///
/// `$?` must be captured as the very first statement of `prompt` (an
/// assignment sets `$?` to true, clobbering it), and is restored before the
/// user's own prompt runs so a status-aware prompt still sees the real result.
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
        Write-Host -NoNewline "$($global:__Tty7Esc)]133;C$($global:__Tty7Bel)"
      }
      $line
    }
  }
}
# --- end tty7 shell integration ---
"#;

/// The redirector files written into our throwaway `ZDOTDIR`. zsh reads its
/// startup files from `$ZDOTDIR`, so for zsh to reach all four of ours we must
/// keep `ZDOTDIR` pointing at *our* dir at every hand-off between files. But
/// while each redirector actually *sources the user's real file* — and once the
/// live session begins — `ZDOTDIR` has to point at the user's real config dir
/// instead: a whole ecosystem of zsh tooling (Zim, oh-my-zsh, `compinit`'s
/// `.zcompdump`) locates its own state via `${ZDOTDIR:-$HOME}`, and if that
/// resolved to our *empty* throwaway dir it would reinstall / rebuild from
/// scratch on every new pane — the 3-second stall and Zim "Installed" spam of
/// issue #15. So each redirector swaps `ZDOTDIR` to the real dir around the
/// `source`, then swaps our dir back so zsh still reaches the next redirector;
/// the integration body ([`ZSH_INTEGRATION`]) restores the real dir for good
/// once every startup file has run.
///
/// The source is done at top level (never wrapped in a function) so the user's
/// config keeps its normal global scope.
fn zsh_redirectors() -> [(&'static str, String); 4] {
    // Run the user's file of the same name with ZDOTDIR aimed at their *real*
    // config dir, then restore ours so zsh reads the next redirector. The real
    // dir is `TTY7_USER_ZDOTDIR`, captured into the env before launch; when it's
    // absent we *unset* ZDOTDIR (not fall back to $HOME) so the file sees exactly
    // what a real launch gives it — an unset ZDOTDIR — and the classic relocate
    // idiom `: ${ZDOTDIR:=~/.config/zsh}` still fires. `tail` runs after the
    // source but before the restore.
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
        // The user's own .zshenv may itself relocate ZDOTDIR — the classic tiny
        // `~/.zshenv` that does `ZDOTDIR=~/.config/zsh`. Capture wherever it points
        // *after* sourcing as the real dir for the later redirectors (and nested
        // tty7); otherwise they'd look under $HOME and miss the user's real config.
        (
            ".zshenv",
            redirect(".zshenv", "export TTY7_USER_ZDOTDIR=${ZDOTDIR:-$HOME}\n"),
        ),
        (".zprofile", redirect(".zprofile", "")),
        // Our integration is appended *after* the user's .zshrc (and after ZDOTDIR
        // is restored to ours) so it extends — not gets clobbered by — the user's
        // PROMPT / hooks.
        (
            ".zshrc",
            format!("{}{ZSH_INTEGRATION}", redirect(".zshrc", "")),
        ),
        (".zlogin", redirect(".zlogin", "")),
    ]
}

/// Environment overrides + spawn adjustments produced by `setup`.
pub struct Injection {
    /// Env vars to add to the child shell's environment.
    pub env: HashMap<String, String>,
    /// Extra argv entries to append after the program (e.g. bash's
    /// `--rcfile <path>`, fish's `-C <script>`). Empty for zsh, which needs no
    /// spawn-time changes at all.
    pub args: Vec<String>,
    /// If set, the caller must spawn the shell as a plain (non-login) process
    /// rather than however it normally would — only bash needs this (see the
    /// module docs), and only when the caller can freely choose the spawn
    /// invocation (i.e. no user-configured custom shell args to preserve).
    pub force_non_login: bool,
    /// The throwaway dir we created, if any; the terminal owns it and removes
    /// it on drop so it doesn't accumulate across sessions. `None` for fish,
    /// which needs no files on disk at all.
    pub dir: Option<PathBuf>,
}

/// Prefix of the throwaway redirector dirs we create under the temp dir (see
/// `setup`). Used to recognize *our own* `ZDOTDIR` when it's inherited.
const ZDOTDIR_PREFIX: &str = "tty7-zdotdir-";

/// True if `path` is one of our own redirector dirs (by basename). When tty7 is
/// launched from inside a tty7 shell, the inherited `ZDOTDIR` already points at
/// such a dir — chaining to it would source a `.zshrc` that doesn't hold the
/// user's real config, dropping their dotfiles (oh-my-zsh, aliases, prompt).
fn is_our_zdotdir(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(ZDOTDIR_PREFIX))
}

/// Resolve the user's *real* ZDOTDIR for the redirectors to source from,
/// surviving nested tty7 launches:
///   1. An outer tty7 may have already exported `TTY7_USER_ZDOTDIR` (the real
///      one it resolved) — trust it, keeping the chain anchored to the user.
///   2. Otherwise use the inherited `ZDOTDIR`, but only if it isn't one of *our*
///      throwaway dirs (which would have no user dotfiles).
///   3. Otherwise `None` → the redirectors fall back to `$HOME`, as zsh would.
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

/// Detected interactive shell kind, resolved from the program tty7 is actually
/// about to spawn (falling back to `$SHELL` when the caller doesn't know it,
/// e.g. because it'll be resolved from the passwd database at spawn time).
enum ShellKind {
    Zsh,
    Bash,
    Fish,
    PowerShell,
}

fn shell_kind(program: Option<&str>) -> Option<ShellKind> {
    let owned = match program {
        Some(p) => p.to_string(),
        None => std::env::var("SHELL").ok()?,
    };
    // Lowercase the basename: Windows program names are case-insensitive and
    // carry an `.exe` suffix, so `PowerShell.exe`, `powershell.exe` and `pwsh`
    // must all match. The Unix shells are conventionally lowercase already, so
    // this only ever normalizes the Windows spellings.
    let base = Path::new(&owned)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    match base.as_str() {
        "zsh" => Some(ShellKind::Zsh),
        "bash" => Some(ShellKind::Bash),
        "fish" => Some(ShellKind::Fish),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => Some(ShellKind::PowerShell),
        _ => None,
    }
}

/// A unique throwaway dir under the OS temp dir, prefixed for later
/// recognition (see `is_our_zdotdir`), one *per pane*. We avoid Date/random by
/// combining the process id with a monotonic counter: the daemon is one
/// long-lived process that spawns many panes, so keying on pid alone would
/// have every pane share a single dir, and the first pane's cleanup (removed
/// on drop) would yank the integration files out from under all the others
/// still running.
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
    // Preserve the user's real ZDOTDIR so our redirectors can source from it;
    // when unset they fall back to $HOME, as zsh itself would. Crucially this
    // resolves correctly under *nested* tty7 (launching tty7 from a tty7 shell):
    // the inherited ZDOTDIR there points at an outer redirector dir of ours, not
    // the user's config — `real_user_zdotdir` sees through that. We always
    // (re)export it so deeper nesting stays anchored to the same real dir.
    if let Some(user_zdotdir) = real_user_zdotdir() {
        env.insert("TTY7_USER_ZDOTDIR".to_string(), user_zdotdir);
    }
    env.insert("ZDOTDIR".to_string(), dir.to_string_lossy().into_owned());

    Some(Injection {
        env,
        args: Vec::new(),
        force_non_login: false,
        dir: Some(dir),
    })
}

/// fish reads `-C`/`--init-command` after its own (untouched) `config.fish`, so
/// there's nothing to write to disk or redirect — the whole body is just an
/// extra argv entry.
fn setup_fish() -> Option<Injection> {
    Some(Injection {
        env: HashMap::new(),
        args: vec!["-C".to_string(), FISH_INTEGRATION.to_string()],
        force_non_login: false,
        dir: None,
    })
}

/// PowerShell reads `-EncodedCommand` after loading its own profiles, so — like
/// fish — the whole body is just extra argv, with nothing on disk. We pass it
/// base64-encoded rather than as a plain `-Command` string so an arbitrary
/// script (quotes, `$`, newlines) survives the Windows command line intact, and
/// because an encoded command isn't subject to the script-file execution policy
/// that would otherwise block a dot-sourced `.ps1` on a stock Windows install.
/// `-NoLogo` drops the startup banner; `-NoExit` keeps the session interactive
/// after the command runs.
fn setup_powershell() -> Option<Injection> {
    Some(Injection {
        env: HashMap::new(),
        args: vec![
            "-NoLogo".to_string(),
            "-NoExit".to_string(),
            "-EncodedCommand".to_string(),
            powershell_encoded_command(POWERSHELL_INTEGRATION),
        ],
        force_non_login: false,
        dir: None,
    })
}

/// Encode a PowerShell script for `-EncodedCommand`, which expects base64 of the
/// command's UTF-16LE bytes. Hand-rolled (both steps) rather than pulling in a
/// base64 crate for this single call site.
fn powershell_encoded_command(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64_encode(&utf16le)
}

/// Standard base64 (RFC 4648) with `=` padding.
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

/// Bash rcfile content: replay the login-shell startup-file chain (since we're
/// about to force a non-login spawn so `--rcfile` takes effect at all — see the
/// module docs), then append the integration body.
fn bash_rcfile() -> String {
    format!(
        r#"
# Replays what a real *login* shell would have sourced, in the same order —
# necessary because tty7 spawns bash non-login (see shell_integration.rs) so
# that --rcfile is honored at all; bash silently ignores it for login shells.
if [[ -f /etc/profile ]]; then source /etc/profile; fi
if [[ -f ~/.bash_profile ]]; then
  source ~/.bash_profile
elif [[ -f ~/.bash_login ]]; then
  source ~/.bash_login
elif [[ -f ~/.profile ]]; then
  source ~/.profile
fi
if [[ -f ~/.bashrc ]]; then source ~/.bashrc; fi
{BASH_INTEGRATION}"#
    )
}

/// Force a non-login bash with our rcfile, replaying the login-file chain
/// ourselves inside it (see [`bash_rcfile`]) since `--rcfile` only takes effect
/// on non-login shells in the first place. Only offered when the caller has no
/// user-configured custom args to preserve (`setup`'s `has_custom_args`) — we
/// can't safely guess how `--rcfile <path> -i` should combine with arbitrary
/// user-supplied bash args.
fn setup_bash() -> Option<Injection> {
    let dir = throwaway_dir("tty7-bashrc-")?;
    let rcfile = dir.join("bashrc");
    std::fs::write(&rcfile, bash_rcfile()).ok()?;

    Some(Injection {
        env: HashMap::new(),
        // `--rcfile` (a GNU long option) must precede `-i`: bash 3.2 — still
        // macOS's shipped `/bin/bash` — refuses to parse a long option once a
        // short one has been seen.
        args: vec![
            "--rcfile".to_string(),
            rcfile.to_string_lossy().into_owned(),
            "-i".to_string(),
        ],
        force_non_login: true,
        dir: Some(dir),
    })
}

/// Set up shell integration for a shell tty7 is about to spawn. `program` is
/// the resolved program path/name if the caller already knows it (e.g. the
/// user's configured custom shell, or the default shell resolved from the
/// passwd database) — passing it, rather than relying on `$SHELL`, is what
/// makes detection correct when they disagree. `has_custom_args` should be
/// `true` when the caller is about to pass user-configured shell args it can't
/// safely override (only affects bash — see [`setup_bash`]).
///
/// Returns the env/arg overrides and the temp dir to clean up, or `None` when
/// the shell isn't supported or anything goes wrong — in which case the
/// terminal launches bare, exactly as before (integration is best-effort).
pub fn setup(program: Option<&str>, has_custom_args: bool) -> Option<Injection> {
    let mut injection = match shell_kind(program)? {
        ShellKind::Zsh => setup_zsh(),
        ShellKind::Fish => setup_fish(),
        ShellKind::Bash if !has_custom_args => setup_bash(),
        ShellKind::Bash => None,
        // PowerShell's `-EncodedCommand` is mutually exclusive with a
        // user-supplied `-Command`/`-File`, so — like bash — don't second-guess
        // a custom-arg invocation; launch it bare.
        ShellKind::PowerShell if !has_custom_args => setup_powershell(),
        ShellKind::PowerShell => None,
    }?;

    // Reset the install-guard sentinel for the shell we're about to spawn. Each
    // integration body sets e.g. `TTY7_SHELL_INTEGRATION=1` and *exports* it, so
    // it leaks to every child process — including a tty7 launched from inside a
    // tty7 shell, and (crucially) the persistent daemon, which inherits it and
    // would otherwise hand it to every shell it spawns. Since the PTY child
    // inherits our process env, that stale `1` makes the guard skip the install
    // → no OSC 133 → no inline line editor. Every shell tty7 spawns is a
    // fresh top-level interactive shell that *should* install the hooks, so we
    // blank the sentinel at this spawn boundary (empty still satisfies the
    // guard's emptiness check); the body re-exports `1` for that shell's own
    // descendants.
    injection
        .env
        .insert("TTY7_SHELL_INTEGRATION".to_string(), String::new());

    Some(injection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_our_zdotdir_matches_only_our_prefix() {
        // A dir we created (basename carries the tty7 prefix) is recognized.
        assert!(is_our_zdotdir("/tmp/tty7-zdotdir-1234-0"));
        assert!(is_our_zdotdir("tty7-zdotdir-x"));
        // The user's real dirs and unrelated paths are not ours.
        assert!(!is_our_zdotdir("/home/alice/.config/zsh"));
        assert!(!is_our_zdotdir("/tmp/other-zdotdir"));
        assert!(!is_our_zdotdir(""));
        // A component that only contains the prefix mid-name is not a match.
        assert!(!is_our_zdotdir("/tmp/not-tty7-zdotdir-1"));
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
        // PowerShell, in every spelling: bare and `.exe`, Windows PowerShell and
        // pwsh 7+, and case-insensitively (Windows program names ignore case).
        // Paths use `/` so `Path::file_name` splits them the same on every host;
        // backslash separators are `std::path`'s job and only split on Windows.
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
        // Unknown shells (and absolute paths to them) resolve to None.
        assert!(shell_kind(Some("/bin/sh")).is_none());
        assert!(shell_kind(Some("cmd.exe")).is_none());
    }

    #[test]
    fn zsh_redirectors_source_user_files_and_append_integration() {
        let files = zsh_redirectors();
        assert_eq!(files.len(), 4);
        let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, [".zshenv", ".zprofile", ".zshrc", ".zlogin"]);
        for (name, body) in &files {
            // Every redirector sources the user's real file of the same name,
            // resolved via the captured real ZDOTDIR (`$TTY7_USER_ZDOTDIR`, or
            // $HOME when the user never set one).
            assert!(
                body.contains("$TTY7_USER_ZDOTDIR"),
                "{name} should reference the user's real ZDOTDIR"
            );
            assert!(body.contains(name), "{name} should source its own name");
            assert!(body.contains("source"), "{name} should source");
        }
        // Only .zshrc carries our integration body (so it extends the user's PROMPT).
        let zshrc = &files[2].1;
        assert!(zshrc.contains("__tty7_precmd"));
        assert!(zshrc.contains("133;A"));
        assert!(!files[0].1.contains("__tty7_precmd"));
    }

    #[test]
    fn zsh_redirectors_point_zdotdir_at_the_real_dir_only_while_sourcing() {
        // Issue #15: ZDOTDIR must resolve to the user's *real* config dir while
        // their startup files run — Zim/oh-my-zsh/compinit key their install state
        // off ${ZDOTDIR:-$HOME}, and our throwaway dir is empty, so leaving ZDOTDIR
        // pointed there makes them reinstall on every pane. Each redirector must:
        //   1. stash our dir, 2. aim ZDOTDIR at the real dir, 3. source, then
        //   4. restore our dir so zsh still finds the *next* redirector.
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
        // The canonical layout is a tiny ~/.zshenv that does `ZDOTDIR=~/.config/zsh`,
        // with the real config living there. After sourcing the user's .zshenv we
        // must capture wherever ZDOTDIR now points so the .zprofile/.zshrc/.zlogin
        // redirectors source from the *relocated* dir (and nested tty7 sees it too),
        // rather than falling back to $HOME and dropping the user's config.
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
        // Only .zshenv recaptures; the other three just source and restore.
        for (name, body) in &files[1..] {
            assert!(
                !body.contains("export TTY7_USER_ZDOTDIR"),
                "{name} must not re-export TTY7_USER_ZDOTDIR"
            );
        }
    }

    #[test]
    fn zsh_integration_restores_real_zdotdir_after_startup() {
        // With every startup file read, the integration body must hand ZDOTDIR back
        // to the user's real dir for the live session (runtime ${ZDOTDIR:-$HOME}
        // lookups, a nested plain `zsh`). It's a one-shot precmd hook that unhooks
        // itself so it doesn't re-fire on every prompt.
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
        // Our integration (bash-preexec derived) is appended.
        assert!(rc.contains("__tty7"));
        assert!(rc.contains("133;"));
    }

    #[test]
    fn every_integration_guards_install_on_empty_sentinel() {
        // `setup()` resets TTY7_SHELL_INTEGRATION to an empty-but-exported "" at each
        // spawn boundary (never *unsets* it), so every shell's install-once guard must
        // key off the sentinel being *empty*, i.e. the `-z "$TTY7_SHELL_INTEGRATION"`
        // idiom shared by zsh/bash. Fish once used `not set -q TTY7_SHELL_INTEGRATION`
        // (definedness), and fish reports an empty exported var as *set* — so the guard
        // was false on every launch and OSC 133 never armed. All three must share the
        // emptiness test so the reset installs a fresh top-level shell while an inherited
        // `1` still blocks re-install.
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
        // Fish specifically must not regress to the definedness test that broke it: an
        // empty exported sentinel reads as *set*, which would skip the install.
        assert!(
            !FISH_INTEGRATION.contains("set -q TTY7_SHELL_INTEGRATION"),
            "fish must guard on emptiness (`test -z`), never `set -q`",
        );
    }

    #[test]
    fn d_emitter_is_prepended_ahead_of_user_precmd_hooks() {
        // The app only switches back to prompt-editing mode when `133;D` arrives.
        // If D waited for the user's whole precmd chain (git-status prompts,
        // conda — easily 100ms+), keys typed right after a command finished
        // would be passed raw to the PTY and kernel-echoed into the grid — the
        // stray-char + PROMPT_SP `%` artifact. So zsh/bash must emit D from a
        // dedicated hook *prepended* to precmd_functions, while the rest of the
        // bookkeeping (cwd, A, the PS1 B marker) stays appended/last.
        assert!(
            ZSH_INTEGRATION.contains("precmd_functions=(__tty7_precmd_d $precmd_functions)"),
            "zsh must prepend the D emitter (add-zsh-hook can only append)"
        );
        assert!(
            BASH_INTEGRATION
                .contains(r#"precmd_functions=(__tty7_precmd_d "${precmd_functions[@]}")"#),
            "bash must prepend the D emitter"
        );
        // D comes from exactly one hook per shell — a second emission site would
        // double-fire on every prompt.
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
        // Regression: the daemon percent-DECODES the OSC 7 payload, so the
        // reporters must escape a literal `%` in `$PWD` as %25 — otherwise a
        // real dir like `/tmp/a%20b` is recorded as `/tmp/a b` (and `%2F`
        // rewrites the path *structure*), breaking cwd-inheriting new tabs and
        // session restore.
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
    }

    #[test]
    fn setup_fish_injects_startup_command_without_files() {
        let inj = setup_fish().expect("fish injection is infallible");
        assert_eq!(inj.args[0], "-C");
        assert!(inj.args[1].contains("__tty7"));
        assert!(inj.args[1].contains("133;"));
        assert!(inj.env.is_empty());
        assert!(!inj.force_non_login);
        // fish needs no throwaway dir on disk.
        assert!(inj.dir.is_none());
    }

    #[test]
    fn setup_zsh_writes_redirectors_and_points_zdotdir_at_them() {
        let inj = setup_zsh().expect("zsh setup should succeed");
        let dir = inj.dir.clone().expect("zsh needs a throwaway dir");
        // ZDOTDIR points the shell at our throwaway dir.
        assert_eq!(
            inj.env.get("ZDOTDIR").map(String::as_str),
            Some(dir.to_string_lossy().as_ref())
        );
        assert!(!inj.force_non_login);
        assert!(inj.args.is_empty());
        // All four redirector files landed on disk with the expected content.
        for (name, body) in zsh_redirectors() {
            let written = std::fs::read_to_string(dir.join(name)).expect("redirector written");
            assert_eq!(written, body);
        }
        // The dir basename is recognizable as ours (so a nested launch skips it).
        assert!(is_our_zdotdir(&dir.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_bash_writes_rcfile_and_forces_non_login() {
        let inj = setup_bash().expect("bash setup should succeed");
        let dir = inj.dir.clone().expect("bash needs a throwaway dir");
        // argv is `--rcfile <path> -i`, in that order.
        assert_eq!(inj.args[0], "--rcfile");
        assert_eq!(inj.args[2], "-i");
        assert!(inj.force_non_login);
        // The rc file on disk matches the generated template.
        let rc = std::fs::read_to_string(&inj.args[1]).expect("rcfile written");
        assert_eq!(rc, bash_rcfile());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_dispatches_by_shell_and_sets_sentinel() {
        // zsh → an injection carrying the "already active" sentinel (empty value).
        let inj = setup(Some("zsh"), false).expect("zsh setup");
        assert_eq!(
            inj.env.get("TTY7_SHELL_INTEGRATION").map(String::as_str),
            Some("")
        );
        if let Some(d) = inj.dir {
            let _ = std::fs::remove_dir_all(d);
        }

        // fish → same sentinel, no files.
        let inj = setup(Some("fish"), false).expect("fish setup");
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));

        // bash without custom args → full injection with non-login override.
        let inj = setup(Some("bash"), false).expect("bash setup");
        assert!(inj.force_non_login);
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));
        if let Some(d) = inj.dir {
            let _ = std::fs::remove_dir_all(d);
        }

        // bash WITH custom args → we must not second-guess the user: no injection.
        assert!(setup(Some("bash"), true).is_none());

        // PowerShell without custom args → encoded-command injection, no files.
        let inj = setup(Some("powershell.exe"), false).expect("powershell setup");
        assert!(inj.env.contains_key("TTY7_SHELL_INTEGRATION"));
        assert!(inj.dir.is_none());
        assert!(!inj.force_non_login);

        // PowerShell WITH custom args → `-EncodedCommand` would collide with the
        // user's own `-Command`/`-File`, so we launch bare.
        assert!(setup(Some("pwsh"), true).is_none());

        // Unknown shell → no integration at all.
        assert!(setup(Some("/bin/sh"), false).is_none());
    }

    #[test]
    fn setup_powershell_injects_encoded_command_without_files() {
        let inj = setup_powershell().expect("powershell injection is infallible");
        // `-NoLogo -NoExit -EncodedCommand <base64>`, in that order — the encoded
        // command must come last, since PowerShell treats it as the value.
        assert_eq!(inj.args[0], "-NoLogo");
        assert_eq!(inj.args[1], "-NoExit");
        assert_eq!(inj.args[2], "-EncodedCommand");
        assert_eq!(inj.args.len(), 4);
        // The payload is pure base64 (so it survives the Windows command line and
        // needs no quoting) and decodes, as UTF-16LE, back to our script.
        let b64 = &inj.args[3];
        assert!(
            b64.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'='),
            "encoded command must be pure base64"
        );
        assert_eq!(decode_utf16le_base64(b64), POWERSHELL_INTEGRATION);
        // No throwaway dir, no forced spawn mode, no env of its own.
        assert!(inj.env.is_empty());
        assert!(inj.dir.is_none());
        assert!(!inj.force_non_login);
    }

    #[test]
    fn powershell_integration_emits_every_osc_133_mark_and_cwd() {
        let s = POWERSHELL_INTEGRATION;
        // A/B wrap the returned prompt; C from the readline hook; D with the exit
        // code from the prompt hook; plus the OSC 7 cwd report.
        assert!(s.contains("]133;A"));
        assert!(s.contains("]133;B"));
        assert!(s.contains("]133;C"));
        assert!(s.contains("]133;D;$code"));
        assert!(s.contains("]7;file://"));
        // Guarded on the empty sentinel like the other shells (PowerShell's own
        // idiom for "unset or empty"), so an inherited `1` blocks re-install.
        assert!(s.contains("if (-not $env:TTY7_SHELL_INTEGRATION)"));
        // $? must be captured before $LASTEXITCODE — an assignment resets $?.
        let ok_at = s.find("$ok = $?").expect("captures $?");
        let exit_at = s.find("$lastExit = $LASTEXITCODE").expect("captures exit");
        assert!(ok_at < exit_at, "$? must be read before the exit code");
        // The user's own prompt is preserved and called through, not replaced.
        assert!(s.contains("$global:__Tty7OrigPrompt = $function:prompt"));
        assert!(s.contains("& $global:__Tty7OrigPrompt"));
        // The literal `%` in the cwd is escaped before the payload is built.
        assert!(s.contains(".Replace('%', '%25')"));
    }

    #[test]
    fn powershell_integration_sets_an_osc_title() {
        let s = POWERSHELL_INTEGRATION;
        // Without an OSC 0/2 title every Windows tab stays generic (PowerShell
        // profiles, unlike macOS's default zsh, set no title). The prompt hook
        // must emit an OSC 0 "user@host:dir" title.
        assert!(s.contains("]0;$($env:USERNAME)@$($env:COMPUTERNAME):"));
        // Home is abbreviated to `~`, matching how the other shells' titles read.
        assert!(s.contains("$titlePath = '~'"));
        // The title path uses forward slashes so the tab-label parser (which splits
        // on `/`) can take the last path segment on Windows too.
        assert!(s.contains("$titlePath = $fsPath.Replace('\\', '/')"));
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        // The canonical RFC 4648 §10 test vectors, covering both padding cases.
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
        // A string with a multi-byte char to exercise the UTF-16LE step.
        let script = "Write-Host 'héllo ✓'";
        assert_eq!(
            decode_utf16le_base64(&powershell_encoded_command(script)),
            script
        );
    }

    /// Decode a base64 UTF-16LE string back to a Rust `String` — the inverse of
    /// [`powershell_encoded_command`], used to check the encoder round-trips
    /// without a PowerShell interpreter.
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
            let Some(v) = val(c) else { continue }; // skip padding
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
        // The monotonic counter guarantees distinct dirs even within one process.
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
