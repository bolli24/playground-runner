# Playground Image Contract

**Contract version: 1**

This document defines the interface between the playground runner and a language image. Each
language repo (luga, fluke) builds an image that implements this contract. The runner never knows
how a language compiles or runs programs; it only knows this contract.

Everything in the image has to match the compiler it ships with: the adapter, the examples, and
the linter. Tree-sitter grammars for syntax highlighting are not part of the image; they live in
their own repos and the website bundles them.

If an image changes anything described here in an incompatible way, the contract version must be
bumped (see [Versioning](#versioning)).

## Image metadata

Images declare themselves through OCI labels:

| Label | Example | Required |
|---|---|---|
| `playground.contract` | `1` | yes |
| `playground.language` | `luga` | yes |
| `playground.extensions` | `.luga` | yes, comma-separated list of source file extensions |
| `playground.capabilities` | `multi-file` | yes, may be empty (see [Capabilities](#capabilities)) |
| `playground.views` | `hir,mir,cil` | no, comma-separated names matching `[a-z0-9-]+` (see [Views](#views)) |
| `org.opencontainers.image.revision` | full commit SHA | yes |
| `org.opencontainers.image.source` | `https://github.com/bolli24/luga` | yes |

The runner refuses images whose `playground.contract` it doesn't support and keeps using the
previously working image.

## Image layout

```
/playground/
├── run                 # entrypoint, executable
├── examples/
│   ├── examples.toml   # example index
│   └── ...             # example source files
├── fixtures/
│   ├── fixtures.toml   # conformance fixtures (same format as examples.toml)
│   └── ...
└── linter/
    ├── index.js        # ES module, see "Linter"
    └── ...             # wasm and any glue files it loads
```

`ENTRYPOINT` must be `["/playground/run"]`. The runner passes no arguments.

## Source paths

Files are identified by a relative path using `/` as separator, e.g. `main.luga` or
`modules/vec.luga`. The playground is meant for small programs, so folders are allowed but kept
shallow. A valid path:

- consists of 1 to 3 segments separated by `/`
- has every segment matching `^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$` (so no `.`, `..` or empty
  segments)
- ends in one of the image's `playground.extensions`

The runner rejects requests with invalid paths before starting a container. The same paths are
used in the API, in `examples.toml`/`fixtures.toml`, and as keys passed to the linter.

## Execution environment

The runner starts one container per job. The adapter may assume exactly this environment and
nothing more:

| Path | Access | Contents |
|---|---|---|
| `/work/src` | read-only | the user's files at their [source paths](#source-paths) |
| `/work/stdin` | read-only | the program's stdin (may be empty) |
| `/work/build` | read-write, executable, tmpfs | initial working directory, build output |
| `/tmp` | read-write, tmpfs | scratch space |
| everything else | read-only | the image's root filesystem |

Only `/work/build` allows executing files, so compiled programs must be written there. The
adapter may change the working directory, e.g. to `/work/src` so diagnostics show paths relative
to it.

Environment variables:

| Variable | Value |
|---|---|
| `PLAYGROUND_ENTRY` | entry file's source path, relative to `/work/src` (e.g. `main.luga`) |
| `PLAYGROUND_VIEWS` | comma-separated [views](#views) to print, e.g. `hir,mir`; empty if none |
| `PLAYGROUND_RUN` | `1` to run the program after compiling it, `0` to only compile it |
| `HOME`, `TMPDIR` | `/tmp` |
| `NO_COLOR` | `1` |
| `TERM` | `dumb` |

Further guarantees and restrictions:

- Runs as an unprivileged user (uid/gid 65534), with no capabilities and no network access.
- An init process runs as PID 1 and starts `/playground/run`, so neither the adapter nor the program
  is PID 1. Signals behave normally (Linux ignores default-handled signals for PID 1, which turns
  `abort()` into a crash), and the init process passes the exit status through.
- Runs under gVisor in production and CI. Toolchains must not depend on features gVisor lacks.
- Limits apply to the whole container (compile and run together): wall time, memory, PIDs, CPU,
  and output size. The exact values are runner configuration, not part of the contract.
  Programs and toolchains must not rely on specific limits.
- Container stdin is closed. The program's stdin must be read from `/work/stdin`. Images without
  the `stdin` capability still redirect it, so programs that read stdin get EOF.
- Tools must not write anywhere except `/work/build` and `/tmp`. Tools that write next to their
  input or to fixed paths will fail.

## Phases

`/playground/run` has two phases: **compile** and **run**.

1. **Compile.** Build the program from `/work/src/$PLAYGROUND_ENTRY` (and any other files it
   imports). Compiler output (diagnostics, progress messages) goes to the container's stdout and
   stderr.
2. **Phase marker.** When compiling succeeded, write the marker line to **both** stdout and
   stderr, exactly once each, and flush. With `PLAYGROUND_RUN=0`, exit with `0` instead, without
   the marker.

   ```
   \x1ePLAYGROUND-RUN\x1e\n
   ```

   (`\x1e` is ASCII record separator, followed by a newline.)
3. **Run.** Execute the program with stdin redirected from `/work/stdin`. Prefer `exec` so the
   program becomes the container's main process and its exit status is the container's.

Example shape (not normative):

```sh
#!/bin/sh
set -u
cd /work/src
luga build --out-dir /work/build "$PLAYGROUND_ENTRY"
case $? in
    0) ;;
    3) exit 100 ;;   # compile error
    *) exit 101 ;;   # I/O, codegen, linker, anything unexpected
esac
printf '\036PLAYGROUND-RUN\036\n'
printf '\036PLAYGROUND-RUN\036\n' >&2
exec /work/build/"$(basename "${PLAYGROUND_ENTRY%.*}")" < /work/stdin
```

### How the runner interprets output

The runner splits each stream at the **first** occurrence of the marker line. Everything before
it is compile output, everything after it is program output. A program printing the marker itself
can only affect its own output, never the compile output.

### Exit codes

| Marker seen on stdout | Exit code | Meaning |
|---|---|---|
| no | `0`, with `PLAYGROUND_RUN=0` | compiled; the program wasn't run |
| no | `100` | compile error: the user's program is wrong; compile output explains why |
| no | `101` | toolchain error: the adapter or toolchain is broken (missing linker, internal compiler error, unexpected tool output). **Fails the contract test**, so the image isn't promoted. |
| no | anything else | toolchain error, treated like `101` |
| yes | any | the program's exit code (signals as `128 + n`) |

The adapter maps the toolchain's own exit codes to `100` and `101`, so a toolchain can change its
codes without a contract bump. When unsure, an adapter should prefer `101`: a `100` is normal
playground traffic, a `101` means the image needs fixing.

Timeouts and out-of-memory kills are detected by the runner, not reported by the adapter. The
runner reports which phase was active when it killed the container (marker seen or not).

## Views

Views are intermediate representations the compiler can show next to the program, e.g. a
language's MIR or the assembly it generates. An image lists the views it supports in the
optional `playground.views` label; the names are up to the language. `PLAYGROUND_VIEWS` holds the
ones a job requested, always a subset of the label.

The adapter prints each requested view to stdout during the compile phase, after the compiler's
own output and before the phase marker. A view starts with a header line and ends at the next
header or at the end of the compile output:

```
\x1ePLAYGROUND-VIEW <name>\x1e\n
<the view's text>
```

- The view's text must not contain `\x1e`. The text can include strings from the user's program,
  so the adapter removes the character, e.g. with `tr -d '\036'`. Otherwise a program could fake a
  header or the phase marker.
- A view the compiler couldn't produce, e.g. because type checking failed before the MIR was
  built, is left out. Views are printed on compile errors too, as far as the compiler got.
- The runner keeps at most 256 KiB of each view and marks longer ones as truncated. Compile output
  including views has its own size limit, separate from the program's output; exceeding it kills
  the container like any other output limit. An adapter can cut each view after 256 KiB + 1 byte,
  e.g. with `head -c 262145`, so large programs don't hit that limit.
- Views a job didn't request are ignored.

Adapters ignore `PLAYGROUND_VIEWS` and `PLAYGROUND_RUN` if they don't support them, so images
without views keep working: they never get views requested, and a compile-only job just runs.

Example shape, continuing the one above (not normative):

```sh
for view in $(echo "$PLAYGROUND_VIEWS" | tr ',' ' '); do
    [ -f "/tmp/views/$view.txt" ] || continue
    printf '\036PLAYGROUND-VIEW %s\036\n' "$view"
    tr -d '\036' < "/tmp/views/$view.txt" | head -c 262145
done
```

## Examples and fixtures

`examples/examples.toml` lists the programs shown in the playground's file explorer.
`fixtures/fixtures.toml` lists the conformance cases the contract test runs. Both use the same
format:

```toml
[[program]]
id = "hello"                       # unique, [a-z0-9_-]+
title = "Hello world"              # shown in the UI (examples only)
files = ["hello_world.luga"]       # source paths, relative to the TOML file's directory
entry = "hello_world.luga"         # must be one of `files`
stdin = ""                         # optional, default ""
expect.phase = "run"               # optional, "compile" (expect exit 100) or "run" (default)
expect.stdout = "Hello, world!\n"  # optional, exact match of program stdout
expect.exit_code = 0               # optional, default 0 for phase "run"
expect.timeout = false             # optional, true if the runner must kill it for timing out
expect.views = ["mir"]             # optional, views to request; each must be printed and non-empty
```

With `expect.timeout = true`, `expect.phase` is the phase the timeout must happen in, and
`expect.exit_code` is not checked.

A program with imports lists all its files, e.g. `files = ["import.luga", "modules/vec.luga",
"modules/dir.luga"]`. Programs share one directory, so a file used by several programs (like
`modules/vec.luga`) is stored once and listed by each.

Every example is also run by the contract test, so the examples shown in the playground are known
to work at that commit.

### Required fixtures

Every image must provide these fixture `id`s:

| id | Program | Expectation |
|---|---|---|
| `hello` | prints `Hello, world!` | phase `run`, exact stdout, exit 0 |
| `compile_error` | has a type or syntax error | phase `compile` (exit 100) |
| `exit_code` | exits with code 3 | phase `run`, exit 3 |
| `infinite_loop` | never terminates | `expect.timeout = true`, phase `run` |
| `stdin_echo` | reads stdin, prints it back | only if capability `stdin` |
| `multi_file` | entry imports a file in a subfolder | only if capability `multi-file` |

For an image with `playground.views`, the contract test also compiles `hello` with
`PLAYGROUND_RUN=0` and every declared view. It must exit with `0` without the marker and print each
view, non-empty.

## Capabilities

Optional features an image declares in `playground.capabilities`:

| Capability | Meaning |
|---|---|
| `stdin` | programs can read stdin; the UI shows a stdin input |
| `multi-file` | the entry file can import other files in `/work/src`; otherwise only the entry file is used |

## Linter

`/playground/linter/index.js` is an ES module that wraps the language's wasm linter behind one
API, so the playground treats all languages the same. The module must load its other files
relative to `import.meta.url`.

```ts
export default async function init(): Promise<Linter>;

interface Linter {
  /** files: source path → source. entry: one of the paths. */
  check(files: Record<string, string>, entry: string): Diagnostic[];
}

interface Diagnostic {
  file: string;            // source path of the file the diagnostic belongs to
  from: number;            // start offset, UTF-16 code units
  to: number;              // end offset, UTF-16 code units, >= from
  severity: "error" | "warning" | "info";
  message: string;
}
```

Offsets are UTF-16 code units because that's what CodeMirror uses.

`check` must not throw on bad input. If the linter crashes, return a single error diagnostic on
the entry file at offset 0.

The linter only ever runs in the browser:

- The runner copies `linter/` out of the image and serves it under a URL containing the image's
  revision, e.g. `/v1/assets/luga/<revision>/linter/index.js`. Files at such a URL never change,
  so they are served with long-lived cache headers, and a new image gets new URLs.
- The contract test only checks that `linter/index.js` exists. It does not run the linter.
- The linter's correctness, including UTF-16 offsets for non-ASCII sources, is tested in the
  language repo.

## Versioning

Bump `playground.contract` when an image changes in a way an existing runner can't handle:

- paths, environment variables, the marker, or exit code meanings change
- the `examples.toml` / `fixtures.toml` format changes incompatibly
- the linter module's API changes

These do **not** need a bump:

- new examples or fixtures
- new optional TOML fields, labels, or capabilities that the runner ignores when unknown
- any change to how the toolchain works internally, as long as the adapter keeps the contract

The runner supports a set of contract versions. A runner update that adds a new version must be
deployed before images using it are promoted.

## Implementation notes

Not part of the contract; things the current images need to get right.

### luga

- `luga build` calls `gcc` from `PATH` and extracts the runtime library to the temp dir. The image
  needs `gcc` and binutils, and `TMPDIR=/tmp` covers the extraction.
- `run.sh` uses `luga build`, not `luga run`: `run` passes the program's exit code through, which
  would make a program exiting with `3` look like a compile error.
- `run.sh` builds from `/work/src` with `--out-dir /work/build`, so diagnostics show paths relative
  to the source root. luga prints diagnostics without colors when `NO_COLOR` is set.
- luga's exit codes map to the contract like this:

  | luga | Meaning | Contract |
  |---|---|---|
  | 0 | success | marker, then run the program |
  | 1 | I/O, output directory, runtime extraction | `101` (the runner validates paths, so this is never the user's fault) |
  | 2 | bad command-line arguments | `101` (the adapter called luga wrong) |
  | 3 | compile error: lexing, parsing, type checking, missing main | `100` |
  | 4 | code generation failure | `101` |
  | 5 | linker failure | `101` |
  | other | unexpected | `101` |
- Declares `multi-file`. The `multi_file` fixture imports `lib/greet.luga`; the `import` and `tour`
  examples use `test/modules/`.
- `/// EXIT_CODE=n` and `/// SKIP_TEST` directives in `test/` could generate `examples.toml`
  instead of maintaining it by hand.
- The linter is `crates/luga_wasm`, built for `wasm32-unknown-unknown` and bound with
  `wasm-bindgen --target web --remove-name-section` (the name section is over 90% of the module).
  `playground/linter/index.js` maps its `path` to the contract's `file` and turns errors into a
  diagnostic, since `check` must not throw.

### fluke

- `fluke build` takes a single file, so the image doesn't declare `multi-file`.
- `fluke build` writes to `./target` and prints source paths as given, so `run.sh` copies the
  sources into `/work/build` and builds there. Its progress output is only shown when the build
  fails. The program then runs as `dotnet target/<name>.dll`.
- fluke's exit codes map to the contract like this:

  | fluke | Meaning | Contract |
  |---|---|---|
  | 0 | success | marker, then run the program |
  | 1 | I/O: the source couldn't be read or the output written | `101` |
  | 2 | invalid command-line arguments | `101` |
  | 3 | compile error: lexing, parsing, type checking | `100` |
  | 4 | internal compiler error | `101` |
  | other | unexpected | `101` |

  A program's own exit code comes from `:exit(code)`. A failed `:assert` is an unhandled .NET
  exception, which ends the program with `134` (SIGABRT).
- fluke has no loops, so the `infinite_loop` fixture is naive Fibonacci of 60.
- The compiler is built with the `dotnet/sdk` image (ReadyToRun, since it starts once per job) and
  runs on `dotnet/runtime`, with `DOTNET_NOLOGO=1`, `DOTNET_CLI_TELEMETRY_OPTOUT=1` and
  `DOTNET_EnableDiagnostics=0` so the runtime doesn't create diagnostic sockets in `/tmp`.
- The linter is `fluke.Wasm`, published with the `wasm-tools` workload. `linter/` holds its
  `_framework/` (the .NET runtime and assemblies as wasm, a few MB) and `index.js`, which wraps
  `Linter.Check` and only checks the entry file.

### Neither language

- Neither can read stdin yet, so neither declares `stdin`.
