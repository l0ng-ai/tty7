# Directory Structure

```
.cargo/
  config.toml (5 lines)
.claude/
  skills/
    verify/
      SKILL.md (46 lines)
.github/
  ISSUE_TEMPLATE/
    config.yml (8 lines)
    issue.yml (43 lines)
  scripts/
    bundle-linux.sh (35 lines)
    bundle-macos.sh (127 lines)
    bundle-windows.ps1 (45 lines)
    windows-installer.iss (59 lines)
  workflows/
    ci.yml (67 lines)
    release.yml (106 lines)
  dependabot.yml (22 lines)
scripts/
  bench/
    doom-fire-fps.patch (24 lines)
    fire.sh (41 lines)
    io.sh (27 lines)
    mem.sh (79 lines)
    README.md (142 lines)
    run_one.sh (102 lines)
    setup.sh (46 lines)
  fig-convert/
    .gitignore (8 lines)
    convert.mjs (74 lines)
    README.md (70 lines)
src/
  core/
    actions.rs (13 lines)
    config.rs (897 lines)
    mod.rs (29 lines)
    osc.rs (248 lines)
    session.rs (186 lines)
    shells.rs (440 lines)
    threads.rs (42 lines)
    update.rs (381 lines)
  daemon/
    mod.rs (80 lines)
    pane.rs (2145 lines)
    pidfile.rs (121 lines)
    protocol.rs (609 lines)
    server.rs (646 lines)
    shell_integration.rs (977 lines)
    spawn.rs (511 lines)
    transport.rs (689 lines)
    winproc.rs (274 lines)
  terminal/
    cmd_editor.rs (641 lines)
    completion.rs (732 lines)
    element.rs (1625 lines)
    fps.rs (157 lines)
    fuzzy.rs (228 lines)
    highlight.rs (170 lines)
    history.rs (730 lines)
    hold.rs (265 lines)
    input.rs (788 lines)
    mod.rs (66 lines)
    palette.rs (134 lines)
    remote.rs (1549 lines)
    reverse_search.rs (305 lines)
    search.rs (558 lines)
    signature.rs (347 lines)
    size.rs (34 lines)
    typeahead.rs (352 lines)
    view.rs (4530 lines)
  ui/
    app.rs (1711 lines)
    hints.rs (240 lines)
    home.rs (243 lines)
    keymap.rs (181 lines)
    mod.rs (26 lines)
    palette.rs (409 lines)
    pane.rs (548 lines)
    presets.rs (287 lines)
    settings.rs (1035 lines)
    tab_strip.rs (382 lines)
    theme.rs (242 lines)
  main.rs (307 lines)
.gitignore (28 lines)
build.rs (31 lines)
Cargo.toml (172 lines)
CHANGELOG.md (334 lines)
LICENSE (201 lines)
README.md (148 lines)
README.zh-CN.md (137 lines)
repomix.config.json (32 lines)
```