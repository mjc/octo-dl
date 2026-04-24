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

Explicit delete in the TUI, API, or web UI removes the queued/downloading/error
entry and deletes `{output}.part` plus `{output}.part.meta.json`. It does not
delete completed output files.

`cleanup_on_error = true` removes resume artifacts after recoverable download
errors. It does not remove them for normal cancellation/pause. Final condensed
MAC mismatches always discard resume artifacts because the assembled plaintext
failed integrity verification.

Session summaries distinguish completed file size, bytes fetched from the
network during the current run, and bytes reused from partial files. Speed
metrics are based on network bytes only.
