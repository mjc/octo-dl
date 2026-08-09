# octo-dl

super quickly hacked this up last night because I was tired of the other tools that exist.

it's broken af still and the code is terrible. please disregard.

## Resume behavior

`--resume` resumes an interrupted octo-dl session: saved credentials, URL
fetch state, and queued file entries. Chunk resume is separate and happens
automatically for each file when `{output}.part` and/or
`{output}.part.meta.json` exist.

While a file is downloading, octo-dl writes plaintext into `{output}.part`.
Verified MEGA chunk MACs are saved in `{output}.part.meta.json`. Normal
pause/cancel paths preserve both files and force a final metadata save before
the downloader stops, so the next run can skip verified chunks cheaply.

If the metadata sidecar is missing, corrupt, stale, or contains no reusable
chunks, octo-dl scans the existing `.part` file by MEGA chunk boundaries and
writes a fresh sidecar for any full chunks it can salvage. `--force` ignores
resume state and starts fresh.

Explicit delete in the TUI or API removes the queued/downloading/error
entry and deletes `{output}.part` plus `{output}.part.meta.json`. It does not
delete completed output files.

`cleanup_on_error = true` removes resume artifacts after recoverable download
errors. It does not remove them for normal cancellation/pause. Final condensed
MAC mismatches always discard resume artifacts because the assembled plaintext
failed integrity verification.

Session summaries distinguish completed file size, bytes fetched from the
network during the current run, and bytes reused from partial files. Speed
metrics are based on network bytes only.

## Runtime modes

Run the local terminal UI:

```sh
octo --tui
```

`--tui` and `--headless` look for `./config.toml` by default. Pass
`--config /path/to/config.toml` to override that.

Run a headless service with the loopback remote TUI attach stream:

```sh
octo --headless --tui-listen 127.0.0.1:9723
```

Attach an interactive terminal UI to a running service:

```sh
octo --tui --tui-attach 127.0.0.1:9723
```

## Fake MEGA benchmark harness

The fake-MEGA harness now lives in the regular test/bench workflow:

- integration tests keep the condensed-MAC correctness coverage
- `benches/fake_mega.rs` benchmarks the MEGA download/decrypt/MAC path against a
  local fake `mega.nz` public link with `divan`

The benchmark harness:

- serves pre-encrypted ciphertext from memory
- runs the fake server on its own Tokio runtime
- lets you control client workers with `OCTO_FAKE_MEGA_CHUNKS_PER_FILE`
- lets you control fake-server workers with
  `OCTO_FAKE_MEGA_SERVER_WORKER_THREADS`
- lets you control adjacent MEGA chunks per request with
  `OCTO_FAKE_MEGA_MEGA_CHUNKS_PER_REQUEST`
- accepts `OCTO_FAKE_MEGA_SIZE_MIB`, `OCTO_FAKE_MEGA_OUTPUT_DIR`,
  `OCTO_FAKE_MEGA_SEED`, and `OCTO_FAKE_MEGA_KEEP=1` as additional overrides

Example:

```sh
OCTO_FAKE_MEGA_SIZE_MIB=1024 \
OCTO_FAKE_MEGA_CHUNKS_PER_FILE=4 \
OCTO_FAKE_MEGA_SERVER_WORKER_THREADS=4 \
OCTO_FAKE_MEGA_MEGA_CHUNKS_PER_REQUEST=8 \
nix develop -c cargo bench --bench fake_mega
```

The table below records 10x averages for the fixed memory-backed harness with
`server_worker_threads == chunks_per_file` on this machine:

- CPU: AMD Ryzen 9 5950X 16-Core Processor
- RAM: 64 GiB class system (`MemTotal: 65764688 kB`, about 62.7 GiB visible)

| config (`chunks_per_file` / `mega_chunks_per_request`) | avg throughput |
| --- | ---: |
| `1/1` | `786.19 MB/s` |
| `2/2` | `1259.52 MB/s` |
| `2/4` | `1230.85 MB/s` |
| `4/4` | `1697.79 MB/s` |
| `4/8` | `1740.80 MB/s` |
| `8/8` | `2258.94 MB/s` |

## NixOS module

The flake exports `nixosModules.default`.

The module now manages the `config.toml` it points at by default, so these
NixOS options actually control the running service instead of drifting behind
whatever the binary auto-created on first boot:

The option namespace is still `services.octo-dl.web.*` for compatibility, but
it now configures the API bind/listen settings, bookmarklet helper host, and
optional loopback remote-TUI attach stream.

- `services.octo-dl.web.host`
- `services.octo-dl.web.port`
- `services.octo-dl.downloadDir`
- `services.octo-dl.chunksPerFile`
- `services.octo-dl.concurrentFiles`
- `services.octo-dl.forceOverwrite`
- `services.octo-dl.cleanupOnError`

Recommended secret setup is:

- `services.octo-dl.environmentFile` with `MEGA_EMAIL`, `MEGA_PASSWORD`, and optional `MEGA_MFA`
- `services.octo-dl.apiKeyFile` if you want a fixed API key instead of the auto-generated one

Set `services.octo-dl.manageConfig = false` if you want to own the TOML file
yourself.
