// Fig autocomplete spec -> tty7 signature JSON converter.
//
// Fig ships hundreds of community-maintained command specs (MIT), authored in
// TypeScript and distributed *pre-compiled* as ESM on npm
// (`@withfig/autocomplete`, `build/<cmd>.js`). Each module `export default`s a
// spec object. We can't statically parse the TS (specs build options
// programmatically), so we *execute* each compiled module in Node and snapshot
// the resulting object graph — keeping only the static shape (subcommands,
// options, args, descriptions, static generator `script`s) and dropping every
// function (generator `postProcess`/`custom`, `filterTerm`, `generateSpec`, …).
//
// The output schema mirrors tty7's `terminal::completion::spec` serde model
// one-to-one. Runtime JS (postProcess) is intentionally lost: tty7 runs a
// generator's static `script` and falls back to one-suggestion-per-line.
//
// Usage:
//   node convert.mjs --build-dir <fig build/> --out <dir> git docker ...
//
// Acquire the Fig build dir once with:
//   npm pack @withfig/autocomplete && tar xzf withfig-autocomplete-*.tgz
//   # -> package/build/

import { readFile, writeFile, mkdir, access } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import { resolve, join } from 'node:path';

function parseArgs(argv) {
  const out = { commands: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--build-dir') out.buildDir = argv[++i];
    else if (a === '--out') out.out = argv[++i];
    else out.commands.push(a);
  }
  if (!out.buildDir || !out.out || out.commands.length === 0) {
    console.error('usage: node convert.mjs --build-dir <dir> --out <dir> <cmd>...');
    process.exit(2);
  }
  return out;
}

const arr = (x) => (x == null ? [] : Array.isArray(x) ? x : [x]);
const strList = (x) => arr(x).filter((s) => typeof s === 'string');

// Fig's per-entry `icon`: an emoji, a `fig://icon?type=…` template, or a
// `fig://template?color=…&badge=…`. Kept verbatim; the renderer interprets it.
// Omitted (→ absent from JSON) when not a string, so files stay lean.
const iconOf = (x) => (typeof x.icon === 'string' ? x.icon : undefined);

function normSuggestion(s) {
  if (typeof s === 'string') return { names: [s], description: null };
  if (s && typeof s === 'object') {
    return {
      names: strList(s.name),
      description: typeof s.description === 'string' ? s.description : null,
      icon: iconOf(s),
    };
  }
  return null;
}

// Keep only generators whose command is a static string/array. Fig `script`
// may be `["git","branch"]`, `"git branch"`, or a function(tokens) — drop the
// last. `postProcess` (how to parse output) is a fn we always drop; tty7
// defaults to splitting stdout on newlines.
function normGenerators(g) {
  const out = [];
  for (const gen of arr(g)) {
    if (!gen || typeof gen !== 'object') continue;
    const sc = gen.script;
    let script = null;
    if (Array.isArray(sc) && sc.every((t) => typeof t === 'string')) script = sc;
    else if (typeof sc === 'string') script = sc.split(/\s+/).filter(Boolean);
    if (script && script.length) out.push({ script });
  }
  return out;
}

function normArg(a) {
  if (!a || typeof a !== 'object') return null;
  return {
    name: typeof a.name === 'string' ? a.name : null,
    description: typeof a.description === 'string' ? a.description : null,
    optional: !!a.isOptional,
    variadic: !!a.isVariadic,
    template: strList(a.template), // "filepaths" | "folders" -> tty7 path completion
    suggestions: arr(a.suggestions).map(normSuggestion).filter(Boolean),
    generators: normGenerators(a.generators),
  };
}

function normOption(o) {
  return {
    names: strList(o.name),
    description: typeof o.description === 'string' ? o.description : null,
    args: arr(o.args).map(normArg).filter(Boolean),
    required: !!o.isRequired,
    repeatable: o.isRepeatable === true, // number form (max count) -> treat as single for spike
    hidden: !!o.hidden,
    icon: iconOf(o),
  };
}

let stats;

async function importSpec(buildDir, specName) {
  const file = resolve(buildDir, `${specName}.js`);
  const mod = await import(pathToFileURL(file).href);
  let spec = mod.default;
  if (typeof spec === 'function') return null; // versioned spec (fn) — skip for spike
  return Array.isArray(spec) ? spec[0] : spec;
}

async function normSubcommand(c, ctx, depth) {
  if (!c || typeof c !== 'object') return null;
  const node = {
    names: strList(c.name),
    description: typeof c.description === 'string' ? c.description : null,
    hidden: !!c.hidden,
    icon: iconOf(c),
    options: arr(c.options).map(normOption),
    args: arr(c.args).map(normArg).filter(Boolean),
    subcommands: [],
  };
  for (const sub of arr(c.subcommands)) {
    const n = await normSubcommand(sub, ctx, depth + 1);
    if (n) node.subcommands.push(n);
  }
  // Resolve loadSpec: a string reference to another top-level spec whose
  // subcommands/options graft onto this node (e.g. `git flow` -> git-flow).
  if (typeof c.loadSpec === 'string' && depth < 8) {
    const ref = c.loadSpec;
    if (!ctx.visited.has(ref)) {
      ctx.visited.add(ref);
      try {
        const root = await importSpec(ctx.buildDir, ref);
        if (root) {
          const rn = await normSubcommand(root, ctx, depth + 1);
          if (rn) {
            node.subcommands.push(...rn.subcommands);
            node.options.push(...rn.options);
            if (!node.args.length) node.args = rn.args;
            stats.loadSpec.push(`${node.names[0]} <- ${ref}`);
          }
        }
      } catch (e) {
        stats.loadSpecFailed.push(`${ref}: ${e.message}`);
      }
      ctx.visited.delete(ref);
    }
  }
  return node;
}

async function convertCommand(buildDir, name) {
  stats = { loadSpec: [], loadSpecFailed: [] };
  const root = await importSpec(buildDir, name);
  if (!root) throw new Error(`spec ${name} is a function/empty`);
  const ctx = { buildDir, visited: new Set([name]) };
  const sig = await normSubcommand(root, ctx, 0);
  // A default export that isn't a spec object (e.g. the `index` re-export
  // barrel) normalizes to null — skip it cleanly instead of dereferencing null.
  if (!sig) throw new Error(`spec ${name} produced no signature`);
  sig.name = sig.names[0] ?? name;
  delete sig.names;
  delete sig.hidden;
  return { sig, stats };
}

function count(sig) {
  let subs = 0, opts = 0;
  const walk = (n) => {
    opts += n.options.length;
    subs += n.subcommands.length;
    n.subcommands.forEach(walk);
  };
  walk(sig);
  return { subs, opts };
}

async function main() {
  const { buildDir, out, commands } = parseArgs(process.argv.slice(2));
  await mkdir(out, { recursive: true });
  let ok = 0;
  const failed = [];
  for (const name of commands) {
    // Isolate each command: a spec that throws (a versioned function-default
    // export, a bad import, a malformed graph) must not abort the batch and
    // silently strand every command after it — a single crash near 'i' is
    // exactly what once left kubectl/npm/node/python unconverted.
    try {
      const { sig, stats } = await convertCommand(buildDir, name);
      const json = JSON.stringify(sig); // minified — generated data, not hand-edited
      const outFile = join(out, `${name}.json`);
      await writeFile(outFile, json);
      const { subs, opts } = count(sig);
      console.log(
        `${name}: ${(json.length / 1024).toFixed(1)} KiB  ` +
          `subcommands=${subs} options=${opts}` +
          (stats.loadSpec.length ? `  loadSpec[${stats.loadSpec.join(', ')}]` : '') +
          (stats.loadSpecFailed.length ? `  FAILED[${stats.loadSpecFailed.join('; ')}]` : ''),
      );
      ok++;
    } catch (e) {
      failed.push(`${name}: ${e.message}`);
      console.error(`✗ ${name}: ${e.message}`);
    }
  }
  console.error(
    `\n${ok}/${commands.length} converted` +
      (failed.length ? `, ${failed.length} skipped (see ✗ above)` : ''),
  );
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
