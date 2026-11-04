# playground-runner

HTTP service that compiles and runs untrusted programs for the luga/fluke web playground.

- Each language ships as a Docker image implementing `CONTRACT.md`; the runner knows nothing language-specific
- Programs run with no network, a read-only filesystem, and limits on memory, PIDs and CPU
- Under gVisor (`runsc`) in production
- Images pulled from GHCR; a new image only replaces the current one if its contract version is supported

## API

- `GET /v1/languages`, `GET /v1/languages/{language}`
- `POST /v1/run`
- `GET /healthz`

## Running

```sh
cargo run -- --config runner.toml
```

Local config uses the default Docker runtime. Deployment: `deploy/README.md`.
