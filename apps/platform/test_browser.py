"""Exercise the local workspace against the running API and worker in CI."""
import csv
import hashlib
import shutil
from pathlib import Path

from playwright.sync_api import expect, sync_playwright


def verify_strategy_fields(page):
    catalog = page.request.get("http://127.0.0.1:8765/v1/run-options").json()["strategy_catalog"]
    strategy = page.get_by_label("Strategy", exact=True)
    expect(strategy).to_have_value(catalog["default_strategy"])
    assert strategy.locator("option").evaluate_all("(options) => options.map(option => option.value)") == [
        *(item["id"] for item in catalog["strategies"]), catalog["comparison_id"]
    ]
    page.get_by_text("Execution and strategy settings", exact=True).click()
    selected = {item["id"]: set(item["parameter_ids"]) for item in catalog["strategies"]}
    selected[catalog["comparison_id"]] = {item["id"] for item in catalog["parameters"]}
    for choice, applicable in selected.items():
        strategy.select_option(choice)
        for parameter in catalog["parameters"]:
            field = page.locator(f'input[name="{parameter["id"]}"]')
            if parameter["id"] in applicable:
                expect(field).to_be_visible()
                expect(field).to_be_enabled()
                default = parameter["default"]
                if default is not None:
                    assert field.evaluate("(input) => input.valueAsNumber") == default
            else:
                expect(field).to_be_hidden()
                expect(field).to_be_disabled()
    for item in catalog["strategies"]:
        for fixed in item["fixed_parameters"]:
            expect(page.locator(f'input[name="{fixed["id"]}"]')).to_have_count(0)
    strategy.select_option("DipDCA")
    dip = page.get_by_label("DipDCA lookback (hours)")
    dip.fill("72")
    strategy.select_option("MomentumSMA")
    expect(dip).to_be_disabled()
    strategy.select_option("DipDCA")
    expect(dip).to_have_value("72")
    dip.fill("168")
    page.get_by_text("Execution and strategy settings", exact=True).click()


with sync_playwright() as playwright:
    chrome = shutil.which("google-chrome") or shutil.which("chromium")
    browser = playwright.chromium.launch(executable_path=chrome)
    page = browser.new_page(viewport={"width": 390, "height": 844})
    page.goto("http://127.0.0.1:8765")
    verify_strategy_fields(page)
    page.get_by_label("Market data").set_input_files("data/sample_btc_1h.csv")
    page.get_by_label("Strategy", exact=True).select_option("all")
    page.get_by_label("Initial capital").fill("1800")
    page.get_by_label("Order amount").fill("10")
    page.get_by_label("Daily buy limit").fill("10")
    page.get_by_role("spinbutton", name="Minimum order", exact=True).fill("1")
    page.get_by_role("button", name="Start backtest").click()
    expect(page.get_by_role("status")).to_have_text("succeeded", timeout=90000)
    assert "?run=" in page.url
    successful_url = page.url
    page.reload()
    ledger = page.get_by_role("link", name="Download order ledger")
    expect(ledger).to_be_visible()
    with page.expect_download() as received:
        ledger.click()
    assert Path(received.value.path()).read_bytes() == Path(".e2e/min.csv").read_bytes()
    expect(page.get_by_role("link", name="Download HTML report")).to_be_visible()
    expect(page.get_by_role("link", name="Download replay manifest")).to_be_visible()
    comparison = page.get_by_role("region", name="Strategy comparison")
    portfolios = comparison.get_by_role("table", name="Portfolio results")
    dca = portfolios.get_by_role("row").filter(
        has=page.get_by_role("rowheader", name="DCA", exact=True))
    dip = portfolios.get_by_role("row").filter(
        has=page.get_by_role("rowheader", name="DipDCA", exact=True))
    expect(dca.get_by_role("cell", name="1,875.49", exact=True)).to_be_visible()
    expect(dip.get_by_role("cell", name="1,862.67", exact=True)).to_be_visible()
    expect(dip.get_by_role("cell", name="-12.82", exact=True)).to_be_visible()
    expect(dip.get_by_role("cell", name="879.08", exact=True)).to_be_visible()
    expect(dip.get_by_role("cell", name="386 h", exact=True)).to_be_visible()
    expect(comparison).to_contain_text("Monetary values in USD.")
    expect(portfolios.locator('[aria-sort="descending"]')).to_contain_text("Final equity")
    portfolios.get_by_role("button", name="Sort by Portfolio", exact=True).click()
    expect(portfolios.locator('[aria-sort="ascending"]')).to_contain_text("Portfolio")
    expect(portfolios.get_by_role("rowheader").first).to_have_text("Buy and hold")
    expect(comparison.get_by_role("img", name="Equity over time")).to_be_visible()
    expect(comparison.get_by_role("list", name="Portfolio legend")).to_contain_text("DipDCA")
    expect(comparison.get_by_text("Bar timestamp (UTC)", exact=True)).to_be_visible()

    ledger_filter = comparison.get_by_label("Ledger portfolio")
    expect(ledger_filter).to_have_value("DCA")
    expect(comparison.locator("#order-range")).to_contain_text("Orders 1–25 of")
    comparison.get_by_role("button", name="Next orders").click()
    expect(comparison.locator("#order-range")).to_contain_text("Orders 26–50 of")
    ledger_filter.select_option("DipDCA")
    expect(comparison.locator("#order-range")).to_contain_text("Orders 1–25 of")
    with Path(".e2e/min.csv").open() as stream:
        dip_orders = [row for row in csv.DictReader(stream) if row["strategy"] == "DipDCA"]
    reasons = {"filled": "Filled", "daily_limit": "Skipped · Daily limit",
               "below_min_notional": "Skipped · Below minimum order",
               "insufficient_cash": "Skipped · Insufficient cash"}
    expected = [reasons[row["status"]] for row in dip_orders[:25]]
    expect(comparison.locator("#order-table tbody tr td:nth-child(4)")).to_have_text(expected)
    skipped = next(row["status"] for row in dip_orders if row["status"] != "filled")
    status_filter = comparison.get_by_label("Order status")
    status_filter.select_option(skipped)
    expect(comparison.locator("#order-range")).to_contain_text("Orders 1–")
    rejected = min(25, sum(row["status"] == skipped for row in dip_orders))
    expect(comparison.locator("#order-table tbody tr td:nth-child(4)")).to_have_text([reasons[skipped]] * rejected)
    ledger_filter.select_option("Cash")
    expect(comparison.locator("#order-range")).to_have_text("No orders match the selected filters.")
    expect(comparison.get_by_role("button", name="Next orders")).to_be_disabled()
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    page.get_by_label("Market data").set_input_files(
        {"name": "broken.csv", "mimeType": "text/csv",
         "buffer": b"timestamp,open,high,low,close\n2024-01-01T00:00:00Z,40,40,40,40\n"})
    page.get_by_role("button", name="Start backtest").click()
    expect(page.locator("#error")).to_contain_text("Missing volume column")
    expect(page.get_by_role("status")).to_have_text("succeeded")
    expect(ledger).to_be_visible()
    assert page.url == successful_url
    page.goto("http://127.0.0.1:8765")
    history = page.get_by_role("region", name="Run history")
    older = history.locator(f'a[href="?{successful_url.split("?", 1)[1]}"]')
    older.click()
    expect(ledger).to_be_visible()
    assert page.url == successful_url
    expect(comparison.get_by_role("table", name="Portfolio results")).to_be_visible()

    legacy_id = hashlib.sha256(b"old,bad,dataset\n1,2,3\n").hexdigest()
    page.get_by_label("Saved dataset").select_option(legacy_id)
    page.get_by_role("button", name="Start backtest").click()
    expect(page.get_by_role("status")).to_have_text("failed", timeout=90000)
    expect(page.locator("#diagnostic")).not_to_be_empty()
    expect(ledger).to_have_count(0)
    expect(comparison).to_be_hidden()
    newest = page.request.get("http://127.0.0.1:8765/v1/runs?limit=1").json()
    second = page.request.get(f"http://127.0.0.1:8765/v1/runs?limit=1&before={newest['next']}").json()
    assert newest["runs"][0]["state"] == "failed"
    assert second["runs"][0]["id"] == successful_url.split("run=")[1]
    # A legacy run can be opened with the downloads it originally published.
    def legacy_run(route):
        response = route.fetch()
        data = response.json()
        data["resolved_spec"] = None
        for name in ("summary.json", "equity.json"):
            data["artifacts"].pop(name)
        route.fulfill(response=response, json=data)

    status_url = "http://127.0.0.1:8765/v1/runs/" + successful_url.split("run=")[1]
    page.route(status_url, legacy_run)
    page.goto(successful_url)
    expect(comparison).to_contain_text("Comparison preview is unavailable")
    expect(ledger).to_be_visible()
    expect(page.get_by_role("button", name="Repeat exactly")).to_be_hidden()
    expect(page.get_by_role("button", name="Edit a copy")).to_be_visible()
    page.unroute(status_url, legacy_run)

    dataset = page.get_by_label("Saved dataset")
    option = dataset.locator("option").filter(has_text="PARQUET")
    expect(option).to_have_count(1)
    dataset.select_option(option.get_attribute("value"))
    expect(page.locator("#dataset-note")).to_contain_text("2 hourly bars")
    page.route("**/v1/datasets?*", lambda route: route.abort())
    page.get_by_label("Strategy", exact=True).select_option("DipDCA")
    with page.expect_response(lambda response: response.url.endswith("/v1/runs") and response.status == 202) as received:
        page.get_by_role("button", name="Start backtest").click()
    expect(page).to_have_url("http://127.0.0.1:8765/?run=" + received.value.json()["id"])
    expect(page.get_by_role("status")).to_have_text("succeeded", timeout=90000)
    single = page.get_by_role("region", name="Portfolio performance")
    expect(single).to_contain_text("Monetary values in quote units.")
    single_row = single.get_by_role("table", name="Portfolio results").get_by_role("row").last
    expect(single_row.get_by_role("rowheader")).to_have_text("DipDCA")
    expect(single_row.get_by_role("cell").nth(1)).to_have_text("—")
    expect(single.get_by_role("img", name="Equity over time")).to_be_visible()
    page.goto(successful_url)
    repeat = page.get_by_role("button", name="Repeat exactly")
    edit = page.get_by_role("button", name="Edit a copy")
    expect(repeat).to_be_enabled()
    original = page.request.get(status_url).json()
    assert original["resolved_spec"] is not None
    repeat_url = status_url + "/repeat"
    def reject_repeat(route):
        route.fulfill(status=409, json={"detail": "The original build is unavailable."})

    page.route(repeat_url, reject_repeat)
    repeat.click()
    expect(page.locator("#run-action-error")).to_have_text("The original build is unavailable.")
    expect(repeat).to_be_enabled()
    assert page.url == successful_url
    page.unroute(repeat_url, reject_repeat)
    with page.expect_response(lambda response: response.url == repeat_url and response.status == 202) as received:
        repeat.click()
    repeated_id = received.value.json()["id"]
    assert repeated_id != original["id"]
    expect(page).to_have_url("http://127.0.0.1:8765/?run=" + repeated_id)
    expect(page.get_by_role("status")).to_have_text("succeeded", timeout=90000)
    repeated = page.request.get("http://127.0.0.1:8765/v1/runs/" + repeated_id).json()
    assert repeated["dataset"] == original["dataset"]
    assert repeated["resolved_spec"] == original["resolved_spec"]

    page.goto(successful_url)
    dataset = page.get_by_label("Saved dataset")
    capital = page.get_by_label("Initial capital")
    capital.fill("555")
    page.get_by_label("Strategy", exact=True).select_option("DCA")
    edit.click()
    expect(dataset).to_have_value(original["dataset"])
    expect(capital).to_have_value("1800")
    expect(page.get_by_label("Strategy", exact=True)).to_have_value("all")
    expect(page.get_by_role("spinbutton", name="Minimum order", exact=True)).to_have_value("1")
    expect(page.locator("#copy-note")).to_contain_text(original["id"][:8])
    assert page.url == successful_url
    capital.fill("1950")
    with page.expect_response(lambda response: response.url == "http://127.0.0.1:8765/v1/runs"
                              and response.request.method == "POST" and response.status == 202) as received:
        page.get_by_role("button", name="Start backtest").click()
    copied_id = received.value.json()["id"]
    assert copied_id not in (original["id"], repeated_id)
    expect(page).to_have_url("http://127.0.0.1:8765/?run=" + copied_id)
    expect(page.get_by_role("status")).to_have_text("succeeded", timeout=90000)
    copied = page.request.get("http://127.0.0.1:8765/v1/runs/" + copied_id).json()
    assert copied["dataset"] == original["dataset"]
    assert copied["options"]["capital"] == 1950
    assert page.request.get(status_url).json()["options"] == original["options"]
    page.get_by_label("Evaluation", exact=True).select_option("windows")
    page.get_by_label("Strategy", exact=True).select_option("MomentumSMA")
    page.get_by_label("Test length").fill("2")
    page.get_by_role("button", name="Start backtest").click()
    windows_view = page.get_by_role("region", name="Walk-forward results")
    expect(windows_view).to_be_visible(timeout=90000)
    expect(windows_view.locator("tbody tr")).to_have_count(4)
    expect(windows_view).to_contain_text("Arithmetic mean")
    window_url = page.url
    page.reload()
    expect(windows_view).to_be_visible()
    page.get_by_role("button", name="Repeat exactly").click()
    expect(page).not_to_have_url(window_url)
    expect(windows_view).to_be_visible(timeout=90000)
    expect(windows_view.locator("tbody tr")).to_have_count(4)
    page.get_by_role("button", name="Edit a copy").click()
    expect(page.get_by_label("Evaluation", exact=True)).to_have_value("windows")
    expect(page.get_by_label("Test length")).to_have_value("2")
    browser.close()
