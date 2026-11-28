import hashlib
import json
import os
import subprocess
import tempfile
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Annotated, Literal
from uuid import UUID, uuid4

from fastapi import FastAPI, Header, HTTPException, Query, Request, Response
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from psycopg import OperationalError
from psycopg.types.json import Jsonb
from pydantic import BaseModel, ConfigDict, Field, create_model, field_validator
from starlette.concurrency import run_in_threadpool

from shared import ARTIFACTS, BINARY, build_id, connect, initialize, option_flags

MAX_BYTES = 8 * 1024 * 1024
LOCAL_RUN_SCOPE = "local"
DATASET_FORMATS = {
    "text/csv": "csv",
    "application/vnd.apache.parquet": "parquet",
}
DATASET_FIELDS = "id, format, symbol, quote_currency, validation"
DatasetID = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
DatasetLabel = Annotated[str | None, Query(pattern=r"^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$")]


def load_strategy_catalog():
    build = build_id()
    result = subprocess.run([str(BINARY), "strategies", "--json"], check=True, capture_output=True, timeout=10)
    catalog = json.loads(result.stdout)
    if catalog["version"] != 1 or build != build_id():
        raise RuntimeError("QBT binary changed or provided an unsupported strategy catalog")
    return build, catalog


BUILD, STRATEGY_CATALOG = load_strategy_catalog()
COMPARISON_ID = STRATEGY_CATALOG["comparison_id"]
STRATEGY_IDS = tuple(strategy["id"] for strategy in STRATEGY_CATALOG["strategies"]) + (COMPARISON_ID,)
STRATEGY_ALIASES = {
    name.lower(): strategy["id"]
    for strategy in STRATEGY_CATALOG["strategies"]
    for name in (strategy["id"], *strategy["aliases"])
}
STRATEGY_ALIASES[COMPARISON_ID.lower()] = COMPARISON_ID


class ExecutionOptions(BaseModel):
    model_config = ConfigDict(extra="forbid", allow_inf_nan=False)
    capital: float = Field(default=10000, gt=0)
    daily_limit: float | None = Field(default=None, gt=0)
    min_notional: float | None = Field(default=None, gt=0)
    limit_includes_fees: bool = False
    fee: float = Field(default=0.001, ge=0, lt=1)
    slippage: float = Field(default=0.0005, ge=0, lt=1)
    risk_free: float = 0.03


    @field_validator("strategy", mode="before", check_fields=False)
    @classmethod
    def canonical_strategy(cls, value):
        if isinstance(value, str) and value.isascii():
            return STRATEGY_ALIASES.get(value.lower(), value)
        return value


def parameter_field(parameter):
    value_type = int if parameter["type"] == "integer" else float
    default = parameter["default"]
    return (value_type | None if default is None else value_type), Field(
        default=default,
        description=parameter["description"],
        ge=parameter.get("minimum"),
        le=parameter.get("maximum"),
        gt=parameter.get("exclusive_minimum"),
        lt=parameter.get("exclusive_maximum"),
    )


Options = create_model(
    "Options",
    __base__=ExecutionOptions,
    strategy=(Literal[STRATEGY_IDS], STRATEGY_CATALOG["default_strategy"]),
    **{parameter["id"]: parameter_field(parameter) for parameter in STRATEGY_CATALOG["parameters"]},
)


Months = Annotated[int, Field(strict=True, ge=1, le=4294967295)]


class Windows(BaseModel):
    model_config = ConfigDict(extra="forbid")
    train_months: Months
    test_months: Months
    step_months: Months


class Submission(BaseModel):
    model_config = ConfigDict(extra="forbid")
    dataset: DatasetID
    options: Options = Field(default_factory=Options)
    windows: Windows | None = None


@asynccontextmanager
async def lifespan(app):
    initialize()
    app.state.build = BUILD
    yield


app = FastAPI(title="QBT local platform", lifespan=lifespan)


WEB = Path(__file__).resolve().parents[1] / "web"
app.mount("/ui", StaticFiles(directory=WEB), name="ui")


@app.get("/", include_in_schema=False)
def workspace():
    return FileResponse(WEB / "index.html")


@app.get("/healthz", include_in_schema=False)
def health():
    try:
        with connect() as db:
            db.execute("SELECT 1")
    except OperationalError:
        raise HTTPException(503, "Database unavailable") from None
    return {"status": "ok"}


@app.get("/v1/run-options")
def run_options():
    return {**Options.model_json_schema(), "strategy_catalog": STRATEGY_CATALOG}


def inspect_dataset(content, file_format):
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / f"input.{file_format}"
        path.write_bytes(content)
        try:
            result = subprocess.run([str(BINARY), "inspect-dataset", "--input", str(path)],
                                    capture_output=True, text=True, errors="replace", timeout=30,
                                    env={**os.environ, "RUST_BACKTRACE": "0"})
        except subprocess.TimeoutExpired as exc:
            raise HTTPException(503, "Dataset validation timed out") from exc
    if result.returncode:
        diagnostic = result.stderr.removeprefix("Error: ").strip()
        raise HTTPException(422, diagnostic[:2000] or "Invalid dataset")
    return json.loads(result.stdout)


def dataset_metadata(row):
    validation = row.pop("validation")
    return {**row, **(validation or dict.fromkeys(("schema_version", "timeframe", "row_count", "start", "end")))}


def store_dataset(content, file_format, symbol, quote_currency):
    dataset_id = hashlib.sha256(content).hexdigest()
    with connect() as db:
        row = db.execute(f"SELECT {DATASET_FIELDS} FROM qbt_datasets WHERE id = %s", (dataset_id,)).fetchone()
    if row and row["format"] == file_format and row["validation"] is not None:
        if symbol is None and quote_currency is None:
            return dataset_metadata(row)
        validation = row["validation"]
    else:
        validation = inspect_dataset(content, file_format)

    with connect() as db:
        # Backfill missing metadata even if a concurrent upload recorded the hash.
        row = db.execute(f"""
            INSERT INTO qbt_datasets (id, csv, format, symbol, quote_currency, validation)
            VALUES (%s, %s, %s, %s, %s, %s)
            ON CONFLICT (id) DO UPDATE SET
                format = EXCLUDED.format,
                symbol = COALESCE(qbt_datasets.symbol, EXCLUDED.symbol),
                quote_currency = COALESCE(qbt_datasets.quote_currency, EXCLUDED.quote_currency),
                validation = COALESCE(qbt_datasets.validation, EXCLUDED.validation)
            WHERE (qbt_datasets.validation IS NULL OR qbt_datasets.format = EXCLUDED.format)
                AND (qbt_datasets.symbol IS NULL OR EXCLUDED.symbol IS NULL
                     OR qbt_datasets.symbol = EXCLUDED.symbol)
                AND (qbt_datasets.quote_currency IS NULL OR EXCLUDED.quote_currency IS NULL
                     OR qbt_datasets.quote_currency = EXCLUDED.quote_currency)
            RETURNING {DATASET_FIELDS}
        """, (dataset_id, content, file_format, symbol, quote_currency, Jsonb(validation))).fetchone()
        if row is None:
            raise HTTPException(409, "Conflicting metadata for this dataset")
    return dataset_metadata(row)


@app.post("/v1/datasets", status_code=201)
async def upload(request: Request, symbol: DatasetLabel = None, quote_currency: DatasetLabel = None):
    media_type = request.headers.get("content-type", "").split(";")[0].strip().lower()
    file_format = DATASET_FORMATS.get(media_type)
    if file_format is None:
        raise HTTPException(415, "Expected text/csv or application/vnd.apache.parquet")
    content = bytearray()
    async for chunk in request.stream():
        if len(content) + len(chunk) > MAX_BYTES:
            raise HTTPException(413, "Dataset exceeds 8 MiB")
        content.extend(chunk)
    if not content:
        raise HTTPException(422, "Empty dataset")
    return await run_in_threadpool(store_dataset, bytes(content), file_format, symbol, quote_currency)


@app.get("/v1/datasets")
def list_datasets():
    with connect() as db:
        rows = db.execute(f"SELECT {DATASET_FIELDS} FROM qbt_datasets ORDER BY id").fetchall()
    return {"datasets": [dataset_metadata(row) for row in rows]}


@app.get("/v1/datasets/{dataset_id}")
def get_dataset(dataset_id: DatasetID):
    with connect() as db:
        row = db.execute(f"SELECT {DATASET_FIELDS} FROM qbt_datasets WHERE id = %s", (dataset_id,)).fetchone()
        if row is None:
            raise HTTPException(404, "Unknown dataset")
    return dataset_metadata(row)


def freeze_run(dataset, options):
    windows = options.get("windows")
    if windows:
        if options["strategy"] == COMPARISON_ID:
            raise HTTPException(422, "Select one strategy for walk-forward")
        if dataset["validation"] is None:
            raise HTTPException(422, "Validate this dataset before evaluating windows")
        required = (windows["train_months"] + windows["test_months"]) * 720
        if dataset["validation"]["row_count"] < required:
            raise HTTPException(422, f"Walk-forward requires at least {required} hourly bars")
    if dataset["validation"] is None:
        return None
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / ("input." + dataset["format"])
        source.write_bytes(dataset.pop("csv"))
        manifest = Path(tmp) / "manifest.json"
        execution = {key: value for key, value in options.items() if key != "windows"}
        flags = option_flags({**execution, "symbol": dataset["symbol"], "quote_currency": dataset["quote_currency"]})
        try:
            result = subprocess.run([str(BINARY), "run", "--input", str(source), "--manifest-only",
                                     "--save-manifest", str(manifest), *flags], capture_output=True, timeout=30)
        except subprocess.TimeoutExpired as exc:
            raise HTTPException(503, "Run resolution timed out") from exc
        if result.returncode:
            raise HTTPException(422, result.stderr.decode(errors="replace")[:2000])
        saved = manifest.read_text()
        if windows:
            saved = json.dumps({**json.loads(saved), "windows": windows})
        return Jsonb({"version": 1, "build": app.state.build, "dataset": dataset_metadata(dataset),
                      "manifest": saved})


@app.post("/v1/runs", status_code=202)
def submit(submission: Submission, idempotency_key: Annotated[list[str] | None, Header()] = None):
    key = None
    if idempotency_key is not None:
        if len(idempotency_key) != 1:
            raise HTTPException(422, "Send exactly one Idempotency-Key header")
        key = idempotency_key[0]
        if not 1 <= len(key) <= 128 or any(not "!" <= char <= "~" or char == "," for char in key):
            raise HTTPException(422, "Idempotency-Key must contain 1–128 visible ASCII characters without commas")

    options = submission.options.model_dump()
    explicit_options = submission.options.model_dump(exclude_unset=True)
    if submission.windows is not None:
        options["windows"] = explicit_options["windows"] = submission.windows.model_dump()
    explicit = Jsonb(explicit_options)
    scope = LOCAL_RUN_SCOPE if key is not None else None
    with connect() as db:
        if key is not None:
            # Serialize submissions for this key before resolving defaults or reading input bytes.
            db.execute("SELECT pg_advisory_xact_lock(hashtext(%s), hashtext(%s))", (scope, key))
            existing = db.execute("""
                SELECT id, state, dataset = %s AND (request_options = %s OR options = %s) AS matches
                FROM qbt_runs WHERE idempotency_scope = %s AND idempotency_key = %s
            """, (submission.dataset, explicit, Jsonb(options), scope, key)).fetchone()
            if existing:
                if not existing["matches"]:
                    raise HTTPException(409, "Idempotency-Key already used for a different run request")
                return {"id": existing["id"], "state": existing["state"]}

        dataset = db.execute(f"SELECT {DATASET_FIELDS}, csv FROM qbt_datasets WHERE id = %s",
                             (submission.dataset,)).fetchone()
        if dataset is None:
            raise HTTPException(404, "Unknown dataset")
        snapshot = freeze_run(dataset, options)
        row = db.execute("""
            INSERT INTO qbt_runs (id, dataset, options, build, idempotency_scope, idempotency_key,
                                  resolved_spec, request_options)
            VALUES (%s, %s, %s, %s, %s, %s, %s, %s) RETURNING id, state
        """, (uuid4(), submission.dataset, Jsonb(options), app.state.build, scope, key, snapshot, explicit)).fetchone()
    return {"id": row["id"], "state": row["state"]}


@app.post("/v1/runs/{run_id}/repeat", status_code=202)
def repeat(run_id: UUID):
    with connect() as db:
        source = db.execute("SELECT build, resolved_spec IS NOT NULL AS frozen FROM qbt_runs WHERE id = %s",
                            (run_id,)).fetchone()
        if source is None:
            raise HTTPException(404, "Unknown run")
        if not source["frozen"]:
            raise HTTPException(409, "This run has no saved specification")
        if source["build"] != app.state.build:
            raise HTTPException(409, "The original build is unavailable.")
        return db.execute("""
            INSERT INTO qbt_runs (id, dataset, options, build, resolved_spec)
            SELECT %s, dataset, options, build, resolved_spec FROM qbt_runs WHERE id = %s
            RETURNING id, state
        """, (uuid4(), run_id)).fetchone()


@app.get("/v1/runs")
def list_runs(limit: Annotated[int, Query(ge=1, le=100)] = 20, before: UUID | None = None):
    with connect() as db:
        params = []
        where = ""
        if before is not None:
            anchor = db.execute("SELECT created_at FROM qbt_runs WHERE id = %s", (before,)).fetchone()
            if anchor is None:
                raise HTTPException(404, "Unknown history cursor")
            where = "WHERE (created_at, id) < (%s, %s)"
            params.extend((anchor["created_at"], before))
        rows = db.execute(f"""
            SELECT id, created_at, state, options->>'strategy' AS strategy, options->'windows' AS windows,
                   options->'capital' AS capital, options->'daily_limit' AS daily_limit
            FROM qbt_runs {where}
            ORDER BY created_at DESC, id DESC LIMIT %s
        """, (*params, limit + 1)).fetchall()
    return {"runs": rows[:limit], "next": rows[limit - 1]["id"] if len(rows) > limit else None}


@app.get("/v1/runs/{run_id}")
def status(run_id: UUID):
    with connect() as db:
        row = db.execute("""
            SELECT r.id, r.dataset, r.options, r.build, r.state, r.error, r.resolved_spec,
                   CASE WHEN r.resolved_spec IS NULL THEN d.symbol ELSE r.resolved_spec #>> '{dataset,symbol}' END AS symbol,
                   CASE WHEN r.resolved_spec IS NULL THEN d.quote_currency ELSE r.resolved_spec #>> '{dataset,quote_currency}' END AS quote_currency
            FROM qbt_runs r JOIN qbt_datasets d ON d.id = r.dataset WHERE r.id = %s
        """, (run_id,)).fetchone()
        if row is None:
            raise HTTPException(404, "Unknown run")
        links = db.execute("SELECT name FROM qbt_artifacts WHERE run_id = %s ORDER BY name", (run_id,)).fetchall() if row["state"] == "succeeded" else []
    if row["resolved_spec"] is not None:
        row["resolved_spec"]["manifest"] = json.loads(row["resolved_spec"]["manifest"])
    row["artifacts"] = {item["name"]: f"/v1/runs/{run_id}/artifacts/{item['name']}" for item in links}
    return row


@app.post("/v1/runs/{run_id}/cancel")
def cancel(run_id: UUID):
    with connect() as db:
        row = db.execute("SELECT state FROM qbt_runs WHERE id = %s FOR UPDATE", (run_id,)).fetchone()
        if row is None:
            raise HTTPException(404, "Unknown run")
        if row["state"] in ("queued", "running"):
            db.execute("""
                UPDATE qbt_runs SET state = 'cancelled', attempt = NULL, expires_at = NULL
                WHERE id = %s
            """, (run_id,))
        elif row["state"] != "cancelled":
            raise HTTPException(409, "Run already finished")
    return {"id": run_id, "state": "cancelled"}


@app.get("/v1/runs/{run_id}/artifacts/{name}")
def download(run_id: UUID, name: str):
    if name not in ARTIFACTS:
        raise HTTPException(404, "Unknown artifact")
    with connect() as db:
        row = db.execute("SELECT content FROM qbt_artifacts WHERE run_id = %s AND name = %s", (run_id, name)).fetchone()
    if row is None:
        raise HTTPException(404, "Artifact unavailable")
    return Response(row["content"], media_type=ARTIFACTS[name], headers={"Content-Disposition": f'attachment; filename="{name}"'})
