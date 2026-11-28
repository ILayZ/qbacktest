import {createComparison} from "./comparison.js";

const form = document.querySelector("form");
const el = id => document.getElementById(id);
const comparison = createComparison(el("comparison"));
const labels = {"report.html": "Download HTML report", "summary.json": "Download portfolio summary", "equity.json": "Download equity curves", "orders.csv": "Download order ledger", "manifest.json": "Download replay manifest"};
let active = new URLSearchParams(location.search).get("run");
let timer;
let generation = 0;
let selectedRun;
let optionsReady = false;
let working = false;
const formDefaults = new Map();

async function request(url, options) {
  const response = await fetch(url, options);
  if (!response.ok) {
    const data = await response.json().catch(() => ({}));
    const detail = data.detail;
    throw new Error(Array.isArray(detail) ? detail.map(x => x.loc.slice(1).join(".") + ": " + x.msg).join("\n") : detail || "HTTP " + response.status);
  }
  return response.json();
}

let nextCursor = null;
let historyRequest = 0;

async function loadHistory(append = false) {
  const token = ++historyRequest;
  const cursor = append ? nextCursor : null;
  const list = el("past-runs");
  el("older").disabled = true;
  try {
    const page = await request("/v1/runs" + (cursor ? "?before=" + encodeURIComponent(cursor) : ""));
    if (token !== historyRequest) return;
    if (!append) list.replaceChildren();
    for (const run of page.runs) {
      const row = document.createElement("li");
      row.dataset.id = run.id;
      const link = document.createElement("a");
      link.href = "?run=" + encodeURIComponent(run.id);
      link.textContent = (run.windows ? "Walk-forward · " : "") + (run.strategy === "all" ? "All strategies" : run.strategy) + " · " + run.id.slice(0, 8);
      const details = document.createElement("span");
      const daily = run.daily_limit == null ? "" : " · daily limit " + run.daily_limit;
      details.textContent = new Date(run.created_at).toLocaleString() + " · capital " + run.capital + daily;
      const state = document.createElement("span");
      state.className = "run-state";
      state.textContent = run.state;
      row.append(link, details, state);
      list.append(row);
    }
    nextCursor = page.next;
    el("older").hidden = !nextCursor;
    el("history-message").textContent = list.children.length ? "" : "No runs yet.";
  } catch (err) {
    if (token === historyRequest) el("history-message").textContent = "Could not load history. " + err.message;
  } finally {
    if (token === historyRequest) el("older").disabled = false;
  }
}

el("history-refresh").onclick = () => loadHistory();
el("older").onclick = () => loadHistory(true);
loadHistory();

async function refresh() {
  const current = ++generation;
  clearTimeout(timer);
  el("retry").hidden = true;
  try {
    const run = await request("/v1/runs/" + encodeURIComponent(active));
    if (current !== generation) return;
    selectedRun = run;
    updateRunActions();
    el("state").textContent = run.state;
    const rowState = el("past-runs").querySelector(`[data-id="${run.id}"] .run-state`);
    if (rowState) rowState.textContent = run.state;
    el("metadata").replaceChildren();
    for (const [label, value] of [["Run ID",run.id], ["Dataset ID",run.dataset],
      ["Symbol",run.symbol], ["Quote currency",run.quote_currency],
      ["Strategy",run.options.strategy], ["Initial capital",run.options.capital]]) {
      if (value == null) continue;
      const key = document.createElement("dt"), item = document.createElement("dd");
      key.textContent = label;
      item.textContent = value;
      el("metadata").append(key, item);
    }
    el("diagnostic").textContent = run.error || "";
    el("artifacts").replaceChildren();
    for (const [name, url] of Object.entries(run.artifacts)) {
      const link = document.createElement("a");
      link.href = url;
      link.textContent = run.options.windows && name === "summary.json" ? "Download window metrics" : labels[name] || name;
      el("artifacts").append(link);
    }
    el("windows-result").hidden = true;
    if (run.options.windows) {
      comparison.reset();
      if (run.artifacts["summary.json"]) {
        const data = await request(run.artifacts["summary.json"]);
        if (current !== generation) return;
        const percent = n => (n * 100).toFixed(2) + "%";
        el("window-average").textContent = "Arithmetic mean of independent window returns: " + percent(data.mean_return);
        const rows = data.windows.map(w => {
          const row = document.createElement("tr");
          for (const value of [w.bounds.test_start, w.bounds.test_last, w.initial_capital, w.final_equity.toFixed(2),
                               percent(w.total_return), percent(w.max_drawdown)]) {
            row.insertCell().textContent = value;
          }
          return row;
        });
        el("window-rows").replaceChildren(...rows);
        el("windows-result").hidden = false;
      }
    } else {
      void comparison.show(run);
    }
    const cancellable = ["queued","running"].includes(run.state);
    el("cancel-run").hidden = !cancellable;
    if (cancellable) timer = setTimeout(refresh, 1000);
  } catch (err) {
    if (current !== generation) return;
    el("diagnostic").textContent = err.message;
    el("retry").hidden = false;
  }
}

function updateRunActions() {
  el("edit-copy").hidden = !selectedRun;
  el("repeat-run").hidden = selectedRun?.resolved_spec == null;
  el("edit-copy").disabled = !optionsReady || working;
  el("repeat-run").disabled = working;
  el("start").disabled = !optionsReady || working;
}

function setWorking(value) {
  working = value;
  updateRunActions();
}

function openRun(run) {
  el("windows-result").hidden = true;
  comparison.reset();
  active = run.id;
  selectedRun = null;
  updateRunActions();
  el("run-action-error").textContent = "";
  el("cancel-error").textContent = "";
  el("cancel-run").hidden = true;
  const url = new URL(location.href);
  url.searchParams.set("run", active);
  history.replaceState(null, "", url);
  loadHistory();
  el("metadata").replaceChildren();
  el("artifacts").replaceChildren();
  el("diagnostic").textContent = "";
  el("state").textContent = run.state;
  return refresh();
}

el("repeat-run").onclick = async () => {
  const source = selectedRun;
  if (working || source?.resolved_spec == null) return;
  setWorking(true);
  el("run-action-error").textContent = "";
  try {
    const run = await request("/v1/runs/" + encodeURIComponent(source.id) + "/repeat", {method:"POST"});
    if (source.id === active) await openRun(run);
  } catch (err) {
    if (source.id === active) el("run-action-error").textContent = err.message;
  } finally {
    setWorking(false);
  }
};

el("edit-copy").onclick = async () => {
  const source = selectedRun;
  if (working || !optionsReady || !source) return;
  setWorking(true);
  el("run-action-error").textContent = "";
  try {
    const strategy = form.elements.strategy;
    if (![...strategy.options].some(option => option.value === source.options.strategy)) {
      throw new Error("This strategy is unavailable in the current catalog.");
    }
    if (!catalog.has(source.dataset)) {
      const dataset = await request("/v1/datasets/" + encodeURIComponent(source.dataset));
      if (source.id !== active) return;
      rememberDataset(dataset);
    }
    const windows = source.options.windows;
    el("evaluation").value = windows ? "windows" : "standard";
    el("evaluation").dispatchEvent(new Event("change"));
    if (windows) {
      el("train-months").value = windows.train_months;
      el("test-months").value = windows.test_months;
      el("step-months").value = windows.step_months;
    }
    strategy.value = source.options.strategy;
    strategy.dispatchEvent(new Event("change"));
    for (const [name, savedDefault] of formDefaults) {
      const input = form.elements.namedItem(name);
      const value = Object.hasOwn(source.options, name) ? source.options[name] : savedDefault;
      if (input.type === "checkbox") input.checked = value;
      else input.value = value ?? "";
      input.dispatchEvent(new Event("input"));
    }
    datasetSelect.value = source.dataset;
    el("file").value = "";
    el("symbol").value = "";
    el("quote-currency").value = "";
    showCoverage();
    el("error").textContent = "";
    el("copy-note").textContent = "Editing a copy of run " + source.id.slice(0, 8) + ".";
    strategy.focus();
  } catch (err) {
    if (source.id === active) el("run-action-error").textContent = err.message;
  } finally {
    setWorking(false);
  }
};

el("cancel-run").onclick = async () => {
  const id = active;
  el("cancel-run").disabled = true;
  el("cancel-error").textContent = "";
  try {
    await request("/v1/runs/" + encodeURIComponent(id) + "/cancel", {method:"POST"});
  } catch (err) {
    if (id === active) el("cancel-error").textContent = err.message;
  } finally {
    if (id === active) await refresh();
    el("cancel-run").disabled = false;
  }
};
el("retry").onclick = refresh;
if (active) refresh();

const catalog = new Map();
const datasetSelect = el("dataset");

function rememberDataset(dataset) {
  const name = [dataset.symbol, dataset.quote_currency].filter(Boolean).join(" / ");
  const label = [name, dataset.format.toUpperCase(), dataset.id.slice(0, 12)].filter(Boolean).join(" · ");
  let option = catalog.get(dataset.id)?.option;
  if (option) option.text = label;
  else {
    option = new Option(label, dataset.id);
    datasetSelect.add(option);
  }
  catalog.set(dataset.id, {dataset, option});
}

function showCoverage() {
  const saved = catalog.get(datasetSelect.value)?.dataset;
  el("dataset-note").textContent = !saved ? "" : saved.schema_version === null
    ? "Saved before upload validation was introduced."
    : `${saved.row_count.toLocaleString()} hourly bars · ${saved.start} to ${saved.end}`;
}

datasetSelect.onchange = () => {
  if (datasetSelect.value) el("file").value = "";
  showCoverage();
};

el("file").onchange = () => {
  if (el("file").files.length) datasetSelect.value = "";
  showCoverage();
};

request("/v1/datasets").then(({datasets}) => {
  for (const dataset of datasets) {
    if (!catalog.has(dataset.id)) rememberDataset(dataset);
  }
}).catch(err => {
  el("dataset-note").textContent = "Could not load saved datasets. " + err.message;
});

function addStrategyParameter(parameter) {
  const input = document.createElement("input");
  input.type = "number";
  input.name = parameter.id;
  input.step = parameter.type === "integer" ? "1" : "any";
  input.required = parameter.default !== null;
  if (parameter.default === null) input.placeholder = "Strategy default";
  else input.value = parameter.default;
  if (parameter.minimum !== undefined) input.min = parameter.minimum;
  if (parameter.maximum !== undefined) input.max = parameter.maximum;
  input.addEventListener("input", () => {
    const value = input.valueAsNumber;
    const minimum = parameter.exclusive_minimum;
    const maximum = parameter.exclusive_maximum;
    input.setCustomValidity(
      value <= minimum ? "Enter a value greater than " + minimum + "." :
      value >= maximum ? "Enter a value less than " + maximum + "." : ""
    );
  });
  const label = document.createElement("label");
  label.title = parameter.description;
  label.append(parameter.label, input);
  const field = document.createElement("div");
  field.append(label);
  el(parameter.advanced ? "strategy-settings" : "strategy-primary").append(field);
  return {field, input};
}

async function setup() {
  const schema = await request("/v1/run-options");
  const catalog = schema.strategy_catalog;
  const strategy = form.elements.strategy;
  strategy.replaceChildren();
  for (const {id} of catalog.strategies) {
    strategy.add(new Option(id, id));
  }
  strategy.add(new Option("Compare all strategies", catalog.comparison_id));
  strategy.value = catalog.default_strategy;
  for (const [name, field] of Object.entries(schema.properties)) {
    const input = form.elements.namedItem(name);
    if (!input || field.default == null) continue;
    if (input.type === "checkbox") input.checked = field.default;
    else input.value = field.default;
  }
  const parameters = catalog.parameters.map(addStrategyParameter);
  function showStrategyFields() {
    const selected = catalog.strategies.find(item => item.id === strategy.value);
    for (const {field, input} of parameters) {
      const applies = strategy.value === catalog.comparison_id || selected?.parameter_ids.includes(input.name);
      field.hidden = !applies;
      input.disabled = !applies;
    }
  }
  el("evaluation").onchange = () => {
    const windows = el("evaluation").value === "windows";
    el("window-options").disabled = !windows;
    el("window-options").hidden = !windows;
    strategy.options[strategy.options.length - 1].disabled = windows;
    if (windows && strategy.value === catalog.comparison_id) strategy.value = catalog.default_strategy;
    showStrategyFields();
  };
  strategy.addEventListener("change", showStrategyFields);
  showStrategyFields();
  for (const input of form.querySelectorAll('input[name][type="number"], input[name][type="checkbox"]')) {
    formDefaults.set(input.name, input.type === "checkbox" ? input.checked : input.value);
  }
  optionsReady = true;
  updateRunActions();
}

form.onsubmit = async event => {
  event.preventDefault();
  if (working || !optionsReady) return;
  const file = el("file").files[0];
  el("error").textContent = "";
  if (!datasetSelect.value && (!file || file.size === 0 || file.size > 8 * 1024 * 1024)) {
    el("error").textContent = "Select a saved dataset or choose a non-empty CSV or Parquet file up to 8 MiB.";
    return;
  }
  const options = {strategy: form.elements.strategy.value, limit_includes_fees: form.elements.limit_includes_fees.checked};
  for (const input of form.querySelectorAll('input[name][type="number"]')) {
    if (!input.disabled && input.value !== "") options[input.name] = input.valueAsNumber;
  }
  setWorking(true);
  try {
    let datasetId = datasetSelect.value;
    if (!datasetId) {
      const contentType = /\.parquet$/i.test(file.name) ? "application/vnd.apache.parquet" : "text/csv";
      const query = new URLSearchParams();
      const symbol = el("symbol").value.trim();
      const quote = el("quote-currency").value.trim();
      if (symbol) query.set("symbol", symbol);
      if (quote) query.set("quote_currency", quote);
      const dataset = await request("/v1/datasets?" + query, {method:"POST", headers:{"Content-Type":contentType}, body:file});
      datasetId = dataset.id;
      rememberDataset(dataset);
      datasetSelect.value = datasetId;
      el("file").value = "";
      showCoverage();
      el("symbol").value = "";
      el("quote-currency").value = "";
    }
    const payload = {dataset: datasetId, options};
    if (el("evaluation").value === "windows") {
      payload.windows = {train_months: el("train-months").valueAsNumber,
                        test_months: el("test-months").valueAsNumber, step_months: el("step-months").valueAsNumber};
    }
    const run = await request("/v1/runs", {method:"POST", headers:{"Content-Type":"application/json"}, body:JSON.stringify(payload)});
    el("copy-note").textContent = "";
    await openRun(run);
  } catch (err) {
    el("error").textContent = err.message;
  } finally {
    setWorking(false);
  }
};
setup().catch(err => { el("error").textContent = "Could not load run options. Reload to retry. " + err.message; });
