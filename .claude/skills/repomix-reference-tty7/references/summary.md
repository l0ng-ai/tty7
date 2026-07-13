This file is a merged representation of a subset of the codebase, containing files not matching ignore patterns, combined into a single document by Repomix.
The content has been processed where content has been compressed (code blocks are separated by ⋮---- delimiter).

# Summary

## Purpose

This is a reference codebase organized into multiple files for AI consumption.
It is designed to be easily searchable using grep and other text-based tools.

## File Structure

This skill contains the following reference files:

| File | Contents |
|------|----------|
| `project-structure.md` | Directory tree with line counts per file |
| `files.md` | All file contents (search with `## File: <path>`) |
| `tech-stacks.md` | Languages, frameworks, and dependencies per package (search with `## Tech Stack: <path>`) |
| `summary.md` | This file - purpose and format explanation |

## Usage Guidelines

- This file should be treated as read-only. Any changes should be made to the
  original repository files, not this packed version.
- When processing this file, use the file path to distinguish
  between different files in the repository.
- Be aware that this file may contain sensitive information. Handle it with
  the same level of security as you would the original repository.

## Notes

- Some files may have been excluded based on .gitignore rules and Repomix's configuration
- Binary files are not included in this packed representation. Please refer to the Repository Structure section for a complete list of file paths, including binary files
- Files matching these patterns are excluded: target/**, dist/**, Cargo.lock, assets/**, repomix-output.*, target/**, dist/**, Cargo.lock, assets/**, repomix-output.*
- Files matching patterns in .gitignore are excluded
- Files matching default ignore patterns are excluded
- Content has been compressed - code blocks are separated by ⋮---- delimiter
- Files are sorted by Git change count (files with more changes are at the bottom)

## Statistics

76 files | 29,369 lines

| Language | Files | Lines |
|----------|------:|------:|
| Rust | 48 | 27,141 |
| Shell | 7 | 457 |
| Markdown | 6 | 877 |
| YAML | 5 | 246 |
| No Extension | 3 | 237 |
| TOML | 2 | 177 |
| ISS | 1 | 59 |
| PATCH | 1 | 24 |
| JavaScript (ESM) | 1 | 74 |
| JSON | 1 | 32 |
| Other | 1 | 45 |

**Largest files:**
- `src/terminal/view.rs` (4,530 lines)
- `src/daemon/pane.rs` (2,145 lines)
- `src/ui/app.rs` (1,711 lines)
- `src/terminal/element.rs` (1,625 lines)
- `src/terminal/remote.rs` (1,549 lines)
- `src/ui/settings.rs` (1,035 lines)
- `src/daemon/shell_integration.rs` (977 lines)
- `src/core/config.rs` (897 lines)
- `src/terminal/input.rs` (788 lines)
- `src/terminal/completion.rs` (732 lines)