"""End-to-end check against a running API and worker using the release binary."""
import csv
import hashlib
import json
import os
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from pathlib import Path
from threading import Barrier
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from uuid import uuid4

from shared import connect

FIXTURES = Path(__file__).with_name("fixtures")
PARQUET = "application/vnd.apache.parquet"

BASE = "http://127.0.0.1:8765"
BINARY = os.environ.get("QBT_BINARY", "target/release/qbt")


def request(path, data=None, content_type="application/json", headers=None, expected_status=None):
    headers = {"Content-Type": content_type, **(headers or {})}
    try:
        with urlopen(Request(BASE + path, data=data, headers=headers), timeout=10) as response:
            if expected_status is not None:
                assert response.status == expected_status
            return response.read()
    except HTTPError as exc:
        if exc.code != expected_status:
            raise
        with exc:
            return exc.read()


def run(dataset, expected, options, **settings):
    payload = json.dumps({"dataset": dataset, "options": options, **settings}).encode()
    job = json.loads(request("/v1/runs", payload))
    url = "/v1/runs/" + job["id"]
    assert job["state"] == "queued"
    for _ in range(120):
        status = json.loads(request(url))
        if status["state"] in ("failed", "succeeded"):
            break
        time.sleep(0.5)
    assert status["state"] == expected, status
    return status


def verify_idempotency():
    source = Path("data/sample_btc_1h.csv").read_bytes()
    dataset = json.loads(request("/v1/datasets", source, "text/csv"))["id"]
    other_dataset = upload((FIXTURES / "hourly.csv").read_bytes())["id"]
    key = uuid4().hex
    payload = {"dataset": dataset}
    equivalent = {"options": {"daily_limit": None, "capital": 10000.0, "strategy": "DCA"}, **payload}

    def submit(body, key, expected=202):
        headers = {} if key is None else {"Idempotency-Key": key}
        return json.loads(request("/v1/runs", json.dumps(body).encode(), headers=headers, expected_status=expected))

    start = Barrier(8, timeout=10)

    def concurrent_submit(index):
        start.wait()
        return submit(payload if index % 2 else equivalent, key)

    with ThreadPoolExecutor(max_workers=8) as executor:
        ids = {job["id"] for job in executor.map(concurrent_submit, range(8))}
    assert len(ids) == 1, ids
    run_id = ids.pop()

    deadline = time.monotonic() + 30
    while True:
        state = json.loads(request(f"/v1/runs/{run_id}"))["state"]
        if state in {"succeeded", "failed", "cancelled"} or time.monotonic() >= deadline:
            break
        time.sleep(0.1)
    assert state == "succeeded", state
    assert submit(equivalent, key) == {"id": run_id, "state": "succeeded"}
    submit({"dataset": dataset, "options": {"capital": 11000}}, key, 409)
    submit({"dataset": other_dataset}, key, 409)
    submit({"dataset": "0" * 64}, key, 409)
    with connect() as db:
        rows = db.execute("SELECT id, attempts FROM qbt_runs WHERE idempotency_key = %s", (key,)).fetchall()
    assert len(rows) == 1 and str(rows[0]["id"]) == run_id and rows[0]["attempts"] == 1, rows

    for invalid in ("", "x" * 129, "has space", "one,two"):
        submit(payload, invalid, 422)
    fresh_key = "z" * 96 + uuid4().hex
    submit({"dataset": "0" * 64}, fresh_key, 404)
    submit({"dataset": dataset, "options": {"capital": 0}}, fresh_key, 422)
    assert submit(payload, fresh_key)["id"] != run_id
    assert submit(payload, None)["id"] != submit(payload, None)["id"]


def verify_frozen_defaults(dataset):
    from api import BUILD, Options, Submission, app, submit
    from pydantic import create_model

    app.state.build = BUILD
    key = uuid4().hex
    original = submit(Submission(dataset=dataset), [key])
    changed = create_model("ChangedDefaults", __base__=Options, capital=(float, 5000.0))
    new_submission = Submission(dataset=dataset, options=changed())
    assert submit(new_submission, [key])["id"] == original["id"]
    new_run = submit(new_submission, None)
    original_url = f"/v1/runs/{original['id']}"
    repeated = json.loads(request(original_url + "/repeat", b"", expected_status=202))
    results = []
    for job in (original, repeated, new_run):
        for _ in range(120):
            status = json.loads(request(f"/v1/runs/{job['id']}"))
            if status["state"] in ("succeeded", "failed"):
                break
            time.sleep(0.5)
        assert status["state"] == "succeeded", status
        results.append(status)
    first, replay, fresh = results
    assert first["resolved_spec"] == replay["resolved_spec"]
    assert first["options"]["capital"] == 10000 and fresh["options"]["capital"] == 5000
    assert first["resolved_spec"]["manifest"]["spec"] != fresh["resolved_spec"]["manifest"]["spec"]
    for name, url in first["artifacts"].items():
        assert request(url) == request(replay["artifacts"][name]), name
    with connect() as db:
        db.execute("UPDATE qbt_runs SET build = 'unavailable' WHERE id = %s", (new_run["id"],))
    request(f"/v1/runs/{new_run['id']}/repeat", b"", expected_status=409)


def verify_windows(dataset):
    config = {"train_months": 1, "test_months": 2, "step_months": 1}
    source = Path("data/sample_btc_1h.csv")
    status = run(dataset, "succeeded", {"strategy": "MomentumSMA"}, windows=config)
    assert status["resolved_spec"]["manifest"]["windows"] == config
    assert set(status["artifacts"]) == {"manifest.json", "summary.json", "report.html"}
    summary_bytes = request(status["artifacts"]["summary.json"])
    summary = json.loads(summary_bytes)
    windows = summary["windows"]
    assert summary["config"] == config and len(windows) == 4
    assert summary["mean_return"] == sum(w["total_return"] for w in windows) / len(windows)
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        output = ["--output", str(tmp / "report.html"), "--summary", str(tmp / "summary.json")]
        subprocess.run([BINARY, "walk-forward", "--input", str(source), "--strategy", "MomentumSMA",
                        "--train", "1", "--test", "2", "--step", "1", *output], check=True)
        assert (tmp / "summary.json").read_bytes() == summary_bytes
        assert (tmp / "report.html").read_bytes() == request(status["artifacts"]["report.html"])
        lines = source.read_text().splitlines()
        for i, window in enumerate(windows):
            start = (i + 1) * 720
            bars = lines[start + 1:start + 1 + 1440]
            assert window["bounds"]["test_start"] == bars[0].split(",")[0]
            assert window["bounds"]["test_last"] == bars[-1].split(",")[0]
            sliced = tmp / "slice.csv"
            sliced.write_text("\n".join([lines[0], *bars]) + "\n")
            subprocess.run([BINARY, "run", "--input", str(sliced), "--strategy", "MomentumSMA", *output], check=True)
            reference = json.loads((tmp / "summary.json").read_bytes())["portfolios"][0]
            for field in ("initial_capital", "final_equity", "total_return", "max_drawdown", "completed_trades"):
                assert window[field] == reference[field], (i, field)
    for change in ({"step_months": 0}, {"train_months": 6}, {"test_months": -1}):
        payload = {"dataset": dataset, "windows": {**config, **change}}
        request("/v1/runs", json.dumps(payload).encode(), expected_status=422)


def upload(content, content_type="text/csv", *, query="", expected=201):
    return json.loads(request("/v1/datasets" + query, content, content_type, expected_status=expected))


def check_invalid_datasets():
    header = b"timestamp,open,high,low,close,volume\n"
    first = b"2024-01-01T00:00:00Z,40,40,40,40,1\n"
    second = b"2024-01-01T01:00:00Z,50,55,50,55,1\n"
    cases = [
        (header, "text/csv", "no bars"),
        (b"not,a,dataset\n1,2,3\n", "text/csv", "Missing timestamp"),
        (header + first + first, "text/csv", "Bar 2: duplicate timestamp"),
        (header + second + first, "text/csv", "Bar 2: timestamp"),
        (header + first + second.replace(b"T01:", b"T02:"), "text/csv", "Bar 2: expected consecutive hourly bars"),
        (header + first + second.replace(b"01:00:00Z", b"01:00:00.000000001Z"), "text/csv", "Bar 2: expected consecutive hourly bars"),
        (header + first.replace(b"2024-01-01T00:00:00Z", b"invalid"), "text/csv", "Bar 1: invalid timestamp"),
        (header + first.replace(b",40,40,40,40,", b",0,40,40,40,"), "text/csv", "Bar 1: open must be finite and positive"),
        (header + first.replace(b",40,40,40,40,", b",NaN,40,40,40,"), "text/csv", "Bar 1: open must be finite and positive"),
        (header + first.replace(b",40,40,40,40,", b",oops,40,40,40,"), "text/csv", "Bar 1: open must be numeric"),
        (header + first.replace(b",1\n", b",-1\n"), "text/csv", "Bar 1: volume must be finite and nonnegative"),
        (header + first.replace(b",1\n", b",\n"), "text/csv", "Bar 1: missing volume"),
        ((FIXTURES / "invalid_ohlc.csv").read_bytes(), "text/csv", "Bar 2: low and high must bound open and close"),
        ((FIXTURES / "invalid_ohlc.parquet").read_bytes(), PARQUET, "Bar 2: low and high must bound open and close"),
    ]
    for data, media_type, diagnostic in cases:
        error = upload(data, media_type, expected=422)
        assert diagnostic in error["detail"], (diagnostic, error)
        dataset = hashlib.sha256(data).hexdigest()
        unknown = json.loads(request(f"/v1/datasets/{dataset}", expected_status=404))
        assert unknown["detail"] == "Unknown dataset"


def verify_strategy_schema():
    catalog = json.loads(subprocess.check_output([BINARY, "strategies", "--json"]))
    schema = json.loads(request("/v1/run-options"))
    assert schema["strategy_catalog"] == catalog
    assert schema["properties"]["strategy"]["enum"] == [
        *(strategy["id"] for strategy in catalog["strategies"]), catalog["comparison_id"]
    ]
    assert schema["properties"]["strategy"]["default"] == catalog["default_strategy"]
    for parameter in catalog["parameters"]:
        field = schema["properties"][parameter["id"]]
        assert field["default"] == parameter["default"]
        value = next(item for item in field["anyOf"] if item["type"] != "null") if "anyOf" in field else field
        assert value["type"] == parameter["type"]
        for bound, key in (
            ("minimum", "minimum"), ("maximum", "maximum"),
            ("exclusive_minimum", "exclusiveMinimum"), ("exclusive_maximum", "exclusiveMaximum"),
        ):
            assert value.get(key) == parameter.get(bound)


if __name__ == "__main__":
    for attempt in range(40):
        try:
            request("/openapi.json")
            break
        except OSError:
            if attempt == 39:
                raise
            time.sleep(0.5)
    verify_strategy_schema()
    options = {"strategy": "all", "capital": 1800, "daily_limit": 10, "order_amount": 10, "min_notional": 1}
    content = Path("data/sample_btc_1h.csv").read_bytes()
    sample = upload(content, query="?symbol=BTC&quote_currency=USD")
    assert sample == {
        "id": hashlib.sha256(content).hexdigest(),
        "format": "csv",
        "symbol": "BTC",
        "quote_currency": "USD",
        "schema_version": 1,
        "timeframe": "1h",
        "row_count": 4320,
        "start": "2024-01-01T00:00:00Z",
        "end": "2024-06-28T23:00:00Z",
    }
    assert upload(content) == sample
    assert upload(content, query="?symbol=ETH", expected=409)["detail"].startswith("Conflicting metadata")
    assert json.loads(request(f"/v1/datasets/{sample['id']}")) == sample
    assert json.loads(request("/v1/datasets"))["datasets"].count(sample) == 1
    status = run(sample["id"], "succeeded", options)
    assert status["symbol"] == "BTC" and status["quote_currency"] == "USD"
    files = status["artifacts"]
    assert request(files["orders.csv"]) == Path(".e2e/min.csv").read_bytes()
    assert request(files["report.html"]) == Path(".e2e/min.html").read_bytes()
    assert json.loads(request(files["manifest.json"]))["spec"]["include_benchmarks"]
    summary = json.loads(request(files["summary.json"]))
    equity = json.loads(request(files["equity.json"]))
    assert summary["version"] == equity["version"] == 1
    assert summary["symbol"] == "BTC" and summary["quote_currency"] == "USD"
    portfolios = {r["name"]: r for r in summary["portfolios"]}
    assert set(portfolios) == {"DCA", "DipDCA", "VolAdjustedDCA", "MomentumSMA", "RegimeSwitch", "Cash", "Buy and hold"}
    assert round(portfolios["DCA"]["final_equity"], 2) == 1875.49
    dip = portfolios["DipDCA"]
    assert round(dip["final_equity"], 2) == 1862.67
    assert round(dip["difference_vs_dca"], 2) == -12.82
    assert round(dip["unspent_reserve"], 2) == 879.08 and dip["max_reserve_wait_hours"] == 386
    source = Path("data/sample_btc_1h.csv")
    with source.open() as stream:
        times = [datetime.fromisoformat(row["timestamp"]) for row in csv.DictReader(stream)]
    assert [datetime.fromisoformat(t) for t in equity["timestamps"]] == times
    assert {p["name"] for p in equity["portfolios"]} == set(portfolios)
    for curve in equity["portfolios"]:
        assert len(curve["values"]) == len(times)
        assert curve["values"][0] == 1800
        assert curve["values"][-1] == portfolios[curve["name"]]["final_equity"]
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        (tmp / "manifest.json").write_bytes(request(files["manifest.json"]))
        subprocess.run([BINARY, "replay",
                        "--manifest", str(tmp / "manifest.json"), "--input", str(source),
                        "--output", str(tmp / "report.html"), "--summary", str(tmp / "summary.json"),
                        "--equity", str(tmp / "equity.json")], check=True)
        for name in ("summary.json", "equity.json"):
            assert request(files[name]) == (tmp / name).read_bytes()
    alias = run(sample["id"], "succeeded", {"strategy": "voldca", "order_amount": 24})
    assert alias["options"]["strategy"] == "VolAdjustedDCA"
    spec = json.loads(request(alias["artifacts"]["manifest.json"]))["spec"]
    assert spec["strategies"][0]["VolAdjustedDCA"]["base_amount"] == 1

    check_invalid_datasets()

    # The equivalent fixtures have matching coverage and results and distinct content IDs.
    csv_data = (FIXTURES / "hourly.csv").read_bytes()
    csv_id = hashlib.sha256(csv_data).hexdigest()
    with connect() as db:
        inserted = db.execute("""
            INSERT INTO qbt_datasets (id, csv) VALUES (%s, %s) ON CONFLICT DO NOTHING RETURNING id
        """, (csv_id, csv_data)).fetchone()
    if inserted:
        assert json.loads(request(f"/v1/datasets/{csv_id}"))["schema_version"] is None
    parquet_input = FIXTURES / "hourly.parquet"
    parquet = upload(parquet_input.read_bytes(), PARQUET)
    assert parquet["format"] == "parquet" and parquet["row_count"] == 2
    assert parquet["start"] == "2024-01-01T00:00:00Z" and parquet["end"] == "2024-01-01T01:00:00Z"
    assert parquet["id"] == hashlib.sha256(parquet_input.read_bytes()).hexdigest() != csv_id
    reference_options = {"strategy": "DCA", "capital": 1000, "dca_interval": 1, "fee": 0.01, "slippage": 0}
    csv_run = run(csv_id, "succeeded", reference_options)
    csv_metadata = upload(csv_data)
    assert csv_metadata["id"] == csv_id and csv_metadata["format"] == "csv"
    for field in ("schema_version", "timeframe", "row_count", "start", "end"):
        assert csv_metadata[field] == parquet[field]
    parquet_run = run(parquet["id"], "succeeded", reference_options)
    assert json.loads(request(parquet_run["artifacts"]["summary.json"]))["portfolios"][0]["final_equity"] == 1009
    for name, url in parquet_run["artifacts"].items():
        assert request(url) == request(csv_run["artifacts"][name]), name
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        (tmp / "manifest.json").write_bytes(request(parquet_run["artifacts"]["manifest.json"]))
        subprocess.run([BINARY, "replay",
                        "--manifest", str(tmp / "manifest.json"), "--input", str(parquet_input),
                        "--output", str(tmp / "report.html")], check=True)
        assert request(parquet_run["artifacts"]["report.html"]) == (tmp / "report.html").read_bytes()

    # Invalid legacy bytes stay addressable. The worker still reports an error for them.
    legacy = b"old,bad,dataset\n1,2,3\n"
    legacy_id = hashlib.sha256(legacy).hexdigest()
    with connect() as db:
        db.execute("INSERT INTO qbt_datasets (id, csv) VALUES (%s, %s) ON CONFLICT DO NOTHING", (legacy_id, legacy))
    assert json.loads(request(f"/v1/datasets/{legacy_id}"))["schema_version"] is None
    failed = run(legacy_id, "failed", {})
    assert failed["error"] and not failed["artifacts"]
    try:
        request(f"/v1/runs/{failed['id']}/artifacts/report.html")
    except HTTPError as exc:
        assert exc.code == 404
    else:
        raise AssertionError("Published a failed run")

    request(f"/v1/runs/{failed['id']}/repeat", b"", expected_status=409)
    verify_idempotency()
    verify_frozen_defaults(sample["id"])
    verify_windows(sample["id"])
