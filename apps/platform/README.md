# Local strategy testing service

Run the API, one worker, and PostgreSQL 16 on the same trusted host. With Docker Engine 28 or later (or a current Docker Desktop) and the Docker Compose v2 plugin, run this from the repository root.

```bash
docker compose up --build --wait
```

The first build compiles the Rust release CLI and may take several minutes. The API and worker share the resulting image and use the same read-only CLI. Compose waits for PostgreSQL to accept connections and for the API to finish initializing the schema and answer `GET /healthz`. The worker starts after the API becomes healthy. Use `docker compose ps` and `docker compose logs -f api worker postgres` to check the services.

HTTP is published on `127.0.0.1:8765`. Set `QBT_PORT` before starting the stack to use another host port. PostgreSQL has an internal network and no published port. The workspace assumes a single trusted user. The default database password is for local development. To set your own, set `QBT_DB_PASSWORD` to a URL-safe value before the volume is created and keep using that setting on subsequent starts. Changing the setting alone does not alter an existing PostgreSQL role.

The worker executes one simulation at a time with two CPUs, 1 GiB of memory, and 128 MiB of temporary storage. A simulation has a five-minute execution timeout. The API runs one process on one CPU with a 512 MiB memory limit and a Uvicorn concurrency limit of 32. PostgreSQL has one CPU, 512 MiB of memory, and at most 64 connections. Dataset, run, and artifact tables share the `qbt_postgres-data` volume. Plan disk space for the build cache, database, and backups.

Open http://127.0.0.1:8765 in a browser to upload CSV or Parquet data or select a saved dataset, choose a strategy or compare all five, and download results. Open a previous result from Run history or bookmark its URL. History includes runs submitted directly through the API. Use `GET /v1/runs?limit=20` to list recent runs and pass the returned `next` ID as `before` for the following page. The form loads the running build’s strategy catalog from `GET /v1/run-options` and shows the fields for the selected strategy. Comparison shows all available strategy fields. The endpoint includes the existing JSON Schema and a `strategy_catalog` entry identical to `qbt strategies --json`. Strategy IDs and aliases are case-insensitive. Fixed algorithm settings are listed in `fixed_parameters`.

In a second shell, upload the sample and submit a comparison. These examples use jq.

```bash
base=http://127.0.0.1:8765
dataset=$(curl -fsS -H 'Content-Type: text/csv' --data-binary @data/sample_btc_1h.csv "$base/v1/datasets?symbol=BTC&quote_currency=USD" | jq -r .id)
run=$(curl -fsS -H 'Content-Type: application/json' -d "$(jq -nc --arg id "$dataset" '{dataset:$id,options:{strategy:"all",capital:1800,order_amount:10,daily_limit:10}}')" "$base/v1/runs" | jq -r .id)
curl -fsS "$base/v1/runs/$run" | jq
# After state becomes succeeded:
curl -fsS "$base/v1/runs/$run/artifacts/report.html" -o report.html
curl -fsS "$base/v1/runs/$run/artifacts/orders.csv" -o orders.csv
curl -fsS "$base/v1/runs/$run/artifacts/manifest.json" -o manifest.json
```

Interactive endpoint documentation is at `/docs`. Send raw data up to 8 MiB to `POST /v1/datasets` with `Content-Type: text/csv` or `application/vnd.apache.parquet`. QBT validates the bars before saving them. Invalid data returns HTTP 422 with a diagnostic. Failed simulations expose their errors through the run status endpoint.

The catalog uses the SHA-256 of the uploaded bytes as the dataset ID. Uploading the same bytes returns the existing ID. Changing the bytes creates a new entry. Optional `symbol` and `quote_currency` query parameters can fill missing metadata. Changing a recorded value for the same ID returns HTTP 409. Values must start with an ASCII letter or digit and may include periods, underscores, colons, slashes and hyphens, up to 64 characters.

Use `GET /v1/datasets` to list saved datasets and `GET /v1/datasets/{id}` for one entry. The response includes format, symbol, quote currency, validation schema version, timeframe, row count and inclusive UTC start and end. Datasets saved before validation have null validation fields and their IDs remain usable. Reuploading their original bytes records validation metadata if the bars pass.

To retry a submission safely, send an optional `Idempotency-Key` header with `POST /v1/runs`. Use one case-sensitive key of 1 to 128 visible ASCII characters without commas, such as a UUID. The key applies across the local service. A retry with the same dataset ID and validated options, including defaults, returns HTTP 202 with the original run ID and its current state, even after completion. Reusing a key with a different dataset or options returns HTTP 409. Each request without the header creates a new run. Keys are retained with their runs.

For validated datasets, submission resolves the CLI manifest before returning 202. Run details expose `resolved_spec` with its schema version, binary SHA-256, dataset metadata at submission, and engine manifest. The worker replays that manifest against the saved bytes. Repeating the original validated request with the same key returns its existing run even if defaults changed, provided the request still passes validation.

Use **Repeat exactly** or `POST /v1/runs/{id}/repeat` to enqueue a new run with the original snapshot. A different active build or missing snapshot returns 409. **Edit a copy** loads the saved options into the current form for a new submission. Historical jobs and submissions against unvalidated legacy datasets use the original execution path and expose a null snapshot. Reupload the dataset to validate it before creating a new reproducible run.

This first deployment stores inputs, job state and artifacts together in PostgreSQL. Workers claim jobs for their binary's SHA-256. Keep the binary immutable while either process runs and restart both when upgrading. While the CLI is running, the worker renews its lease every second. Another worker of the same build can reclaim the job after the lease expires. Three expired attempts mark a job failed. On Linux, the kernel also stops the CLI subprocess if the worker dies abruptly. Only the current attempt with an active lease can publish, and all artifacts become visible in one transaction. The API supports single, comparison and fixed-parameter walk-forward runs. Project authentication and external object storage are planned.

Choose **Walk-forward** under Evaluation, or submit one strategy and a top-level `"windows": {"train_months": 1, "test_months": 2, "step_months": 1}` with the usual dataset and options. The API requires a validated dataset covering at least one complete window. The worker replays the saved manifest and publishes the HTML report, window metrics in `summary.json`, and replay manifest. The results table shows inclusive UTC test boundaries. Each row starts with fresh capital, budget and indicators. The aggregate return is the arithmetic mean of those independent returns. Use exact repeat, editable copy, cancellation and history as for other runs.

The worker accepts these environment variables, in seconds.

| Variable | Default | Purpose |
| --- | ---: | --- |
| `QBT_WORKER_LEASE_SECONDS` | 30 | Time a claim remains valid after its last renewal |
| `QBT_WORKER_HEARTBEAT_SECONDS` | 1 | Interval for renewing the lease and detecting cancellation |
| `QBT_WORKER_TIMEOUT_SECONDS` | 300 | Hard execution limit for the CLI |

Use a heartbeat shorter than the lease. Increase the timeout for simulations that need more than five minutes. For Compose, set these variables in the shell or the project's `.env` file before starting the stack.

Use **Cancel run** in the workspace or `POST /v1/runs/{id}/cancel` to cancel a queued or running job. A successful cancellation immediately revokes the attempt and prevents artifact publication. The worker checks ownership at each heartbeat during execution. Repeating a cancellation succeeds. If completion won the race, the API returns HTTP 409 and preserves the result.

## Stop, back up, and restore

`docker compose down` removes the containers and retains the PostgreSQL volume. Run the startup command again to resume. Keep the same Compose project name to reuse the volume.

To back up the live database, write a PostgreSQL custom-format dump to a new, private file. Keep a copy on a separate drive or host.

```bash
backup=$(mktemp qbt-backup.XXXXXXXX.dump)
if docker compose exec -T postgres pg_dump -U qbt -d qbt --format=custom > "$backup"; then
  printf '%s\n' "$backup"
else
  rm -- "$backup"
  exit 1
fi
```

Restore into a fresh database to validate the dump. Replace the example archive name and choose an unused database name. `createdb` must succeed before `pg_restore` runs.

```bash
backup=qbt-backup.ABCDEFGH.dump
restore_db=qbt_restore_check
docker compose exec -T postgres createdb -U qbt --template=template0 "$restore_db" &&
docker compose exec -T postgres pg_restore -U qbt -d "$restore_db" \
  --no-owner --no-privileges --single-transaction < "$backup" &&
docker compose exec -T postgres psql -X -v ON_ERROR_STOP=1 -U qbt -d "$restore_db" \
  -c 'SELECT count(*) FROM qbt_datasets;' \
  -c 'SELECT state, count(*) FROM qbt_runs GROUP BY state;' \
  -c 'SELECT count(*) FROM qbt_artifacts;'
```

Compare the restored counts against the expected snapshot before relying on a backup. The Compose workflow checks dataset and artifact hashes after restoring to a fresh database. To recover a deployment, restore into a fresh database as above, stop the API and worker, and set `DATABASE_URL` for both services in a Compose override to use the restored database. Keep the original database available until you verify the recovered deployment.

## Upgrade

Before upgrading QBT, stop the API with `docker compose stop api` to close submissions. Let the old worker finish queued and running jobs. Check for those jobs with:

```bash
docker compose exec -T postgres psql -U qbt -d qbt \
  -c "SELECT state, count(*) FROM qbt_runs WHERE state IN ('queued', 'running') GROUP BY state;"
```

Then stop the worker with `docker compose stop worker`, take a backup, and run the startup command from the new checkout under the same project name. Workers claim jobs for their exact CLI build, so incomplete jobs may need the previous image to finish. On startup, both services initialize the schema and apply the release's schema upgrades transactionally under a PostgreSQL advisory lock, preserving existing runs. For a release with schema changes, rehearse its upgrade on a restored copy and follow that release's migration instructions. Changing the PostgreSQL major version requires a separate database upgrade or dump and restore.

## Machine-readable results

New single and comparison runs publish `summary.json` and `equity.json` alongside the HTML, ledger and manifest. Discover available artifacts through `GET /v1/runs/{id}`. Earlier runs retain their original downloads. For a local run or replay, pass `--summary summary.json --equity equity.json`.

Both JSON documents carry `version: 1`. The summary includes `symbol` and `quote_currency` when supplied with the dataset. The replay manifest records these values so replay reproduces the summary. Summary returns, drawdowns and weights are fractions. Equity, DCA differences and reserves use the input's quote currency. Completed trades count closed trades, while the ledger includes buys and skipped orders. DCA differences are null when DCA is absent. Reserve fields apply to DipDCA. Equity contains one ordered UTC bar timestamp array and one equally long array of close valuations per portfolio, including benchmarks. Values retain engine precision.
