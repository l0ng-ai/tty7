# fig-convert — Fig autocomplete specs → tty7 completion signatures

tty7's per-command completion (flags/subcommands/args with descriptions) is
driven by signature data generated from [Fig's autocomplete spec corpus][fig]
(MIT-licensed, community-maintained, hundreds of commands). This is the
**build-time converter**; the runtime consumer is `src/terminal/signature.rs`.

## Why a converter (and not the JS engine)

Fig specs are authored in TypeScript and shipped **pre-compiled** as ESM on npm.
Each spec `export default`s an object whose *static* shape (subcommands, options,
args, descriptions, static generator `script`s) is exactly what a completion
menu needs. The only truly dynamic parts are functions — generator `postProcess`,
`custom`, `filterTerm`, `generateSpec` — which need a JS runtime to evaluate.

We convert ahead of time: run each spec once, snapshot the
static shape to JSON, and drop the functions. **No JS engine at runtime.** (An
embedded QuickJS evaluating the functions live would be the alternative; we don't need it.) Dynamic
value completion (`git checkout <branch>`) is recovered later by running a
generator's static `script` and splitting stdout on newlines — the `script`s are
already captured; only the executor is future work.

## Regenerate

```bash
# 1. Fetch the compiled spec corpus once (into this dir, gitignored).
npm pack @withfig/autocomplete
tar xzf withfig-autocomplete-*.tgz          # -> package/build/<cmd>.js

# 2. Convert the commands tty7 embeds into assets/completions/.
node convert.mjs --build-dir package/build --out ../../assets/completions git docker
```

`convert.mjs` executes each `<cmd>.js`, walks the object graph keeping only
static fields, resolves `loadSpec` references (e.g. `docker compose` grafts in
the `docker-compose` spec), and writes minified `assets/completions/<cmd>.json`.
Each command is converted in isolation: one spec that throws is reported (`✗`)
and skipped, never aborting the batch — of Fig's ~716 specs, 715 convert and
only a non-command `index` barrel is skipped. Pass as many command names as you
like in one run.

## Output schema

Mirrors the serde model in `src/terminal/signature.rs` one-to-one:

- root: `{ name, description, options[], args[], subcommands[] }`
- subcommand: `{ names[], description, hidden, options[], args[], subcommands[] }`
- option: `{ names[], description, args[], required, repeatable, hidden }`
- arg: `{ name, description, optional, variadic, template[], suggestions[], generators[] }`
- suggestion: `{ names[], description }`
- generator: `{ script[] }`  (static shell command only; `postProcess` dropped)

## Scope & rollout

Signatures are **read from disk**, not embedded: `signature::spec_source` resolves
a `completions/` directory (inside the macOS `.app` bundle's `Resources`, beside
the executable on Linux/Windows, or the in-tree `assets/completions` for
`cargo run`/tests) and lazily loads `<cmd>.json` by command name. So adding a
command is just dropping its JSON into `assets/completions/` — the packaging
scripts copy the whole directory into each bundle, and no recompile or binary
bloat is involved. A big spec is ~300–500 KiB; the full corpus is ~700 commands,
so ship whatever subset you want rather than all of it if bundle size matters.

The in-tree set is ~95 everyday commands (version control, container/k8s,
language package managers, cloud CLIs, shell/sysadmin tools) — ~5.5 MB, curated
from the corpus. The two outliers `aws` (~51 MB) and `gcloud` (~18 MB) are
deliberately excluded; add them only if you also trim them. To add a command,
run `convert.mjs` with its name (step 2 above) and commit the resulting JSON.

[fig]: https://github.com/withfig/autocomplete
