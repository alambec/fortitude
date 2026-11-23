// Fortitude Explorer: search and browse patients, studies, series and instances, query and
// retrieve from modalities, and send to them.
//
// A dependency-free single-page application over QIDO-RS, WADO-RS, WADO-URI and the
// administration API (modalities). Routes live
// in the URL fragment, so that the back button, reloads and shared links keep working.

"use strict";

const meta = (name) => document.querySelector(`meta[name="${name}"]`).content.replace(/\/+$/, "");
const ROOT = meta("dicomweb-root");
const WADO = meta("wado-root");

/** Results per page of studies and series. */
const PAGE = 100;
/** Studies fetched to build the list of patients, which QIDO-RS has no level for. */
const PATIENT_STUDIES = 5000;

const T = {
  PatientName: "00100010",
  PatientID: "00100020",
  IssuerOfPatientID: "00100021",
  PatientBirthDate: "00100030",
  PatientSex: "00100040",
  StudyInstanceUID: "0020000D",
  StudyDate: "00080020",
  StudyTime: "00080030",
  AccessionNumber: "00080050",
  ReferringPhysicianName: "00080090",
  StudyDescription: "00081030",
  StudyID: "00200010",
  ModalitiesInStudy: "00080061",
  NumberOfStudyRelatedSeries: "00201206",
  NumberOfStudyRelatedInstances: "00201208",
  SeriesInstanceUID: "0020000E",
  Modality: "00080060",
  SeriesNumber: "00200011",
  SeriesDescription: "0008103E",
  SeriesDate: "00080021",
  SeriesTime: "00080031",
  BodyPartExamined: "00180015",
  NumberOfSeriesRelatedInstances: "00201209",
  SOPClassUID: "00080016",
  SOPInstanceUID: "00080018",
  InstanceNumber: "00200013",
  NumberOfFrames: "00280008",
  Rows: "00280010",
  Columns: "00280011",
};

/** Study attributes returned with series search results. */
const SERIES_INCLUDE = ["PatientName", "PatientID", "StudyDate", "StudyDescription", "AccessionNumber"];

const MODALITIES = ["CR", "CT", "DX", "ES", "IO", "MG", "MR", "NM", "OP", "OT", "PR", "PT", "PX",
  "RF", "RTDOSE", "RTIMAGE", "RTPLAN", "RTSTRUCT", "SEG", "SM", "SR", "US", "XA", "XC"];

// ---------------------------------------------------------------------------------------------
// DOM

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value == null || value === false) continue;
    if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "class") el.className = value;
    else el.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child == null || child === false) continue;
    el.append(child instanceof Node ? child : String(child));
  }
  return el;
}

const loading = (text = "Loading…") => h("div", { class: "loading" }, text);
const notice = (kind, text) => h("div", { class: `notice ${kind}` }, text);

function copyButton(text) {
  return h("button", {
    class: "ghost small copy",
    title: "Copy to the clipboard",
    onclick: async (e) => {
      e.stopPropagation();
      const button = e.currentTarget;
      try {
        await navigator.clipboard.writeText(text);
        button.textContent = "Copied";
      } catch {
        button.textContent = "Failed";
      }
      setTimeout(() => (button.textContent = "Copy"), 1200);
    },
  }, "Copy");
}

function details(entries) {
  return h("dl", { class: "details" }, entries
    .filter(([, value]) => value != null && value !== "" && !(Array.isArray(value) && !value.length))
    .map(([label, value, opts = {}]) => h("div", {},
      h("dt", {}, label),
      h("dd", { class: opts.mono ? "mono" : null }, value, opts.mono ? copyButton(value) : null))));
}

function badges(values) {
  return values.filter(Boolean).map((v) => h("span", { class: "badge" }, v));
}

function thumbnail(url) {
  const img = h("img", { src: url, loading: "lazy", alt: "" });
  img.addEventListener("error", () => img.replaceWith(h("div", { class: "placeholder" })));
  return img;
}

// ---------------------------------------------------------------------------------------------
// DICOM JSON

function values(item, key) {
  const v = item && item[T[key] || key];
  return (v && v.Value) || [];
}

function text(item, key) {
  const v = values(item, key)[0];
  if (v == null) return "";
  return typeof v === "object" ? v.Alphabetic || "" : String(v);
}

function number(item, key) {
  const v = Number(values(item, key)[0]);
  return Number.isFinite(v) ? v : null;
}

function formatName(value) {
  if (!value) return "";
  const [family = "", given = "", middle = "", prefix = "", suffix = ""] = value.split("^");
  const first = [prefix, given, middle].filter(Boolean).join(" ");
  const name = [family, first].filter(Boolean).join(", ");
  return [name, suffix].filter(Boolean).join(" ");
}

function formatDate(value) {
  const m = /^(\d{4})(\d{2})(\d{2})$/.exec(value || "");
  return m ? `${m[1]}-${m[2]}-${m[3]}` : value || "";
}

function formatTime(value) {
  const m = /^(\d{2})(\d{2})?(\d{2})?/.exec(value || "");
  return m ? [m[1], m[2], m[3]].filter(Boolean).join(":") : value || "";
}

const name = (item) => formatName(text(item, "PatientName"));
const dateTime = (item, date, time) => [formatDate(text(item, date)), formatTime(text(item, time))].filter(Boolean).join(" ");

function formatSize(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let i = 0;
  while (bytes >= 1000 && i < units.length - 1) {
    bytes /= 1000;
    i++;
  }
  return `${bytes.toFixed(i && bytes < 10 ? 1 : 0)} ${units[i]}`;
}

const integer = new Intl.NumberFormat();

// ---------------------------------------------------------------------------------------------
// Server

/** Aborted when the user navigates away, so that stale requests do not pile up. */
let navigation = new AbortController();

class HttpError extends Error {}

async function get(url, accept, signal = navigation.signal) {
  const response = await fetch(url, { headers: { Accept: accept }, signal });
  if (!response.ok) {
    const reason = (await response.text()).trim();
    throw new HttpError(`${response.status} ${response.statusText}${reason ? `: ${reason}` : ""}`);
  }
  return response;
}

/** A QIDO-RS search; `truncated` when the server cut the results at its maximum. */
async function qido(path, params = {}, signal = undefined) {
  const query = new URLSearchParams(Object.entries(params).filter(([, v]) => v != null && v !== ""));
  const search = query.toString();
  const response = await get(`${ROOT}${path}${search ? `?${search}` : ""}`, "application/dicom+json", signal);
  const body = await response.text();
  return { items: body ? JSON.parse(body) : [], truncated: response.headers.has("Warning") };
}

const studyUrl = (study) => `${ROOT}/studies/${encodeURIComponent(study)}`;
const seriesUrl = (study, series) => `${studyUrl(study)}/series/${encodeURIComponent(series)}`;
const instanceUrl = (study, series, instance) => `${seriesUrl(study, series)}/instances/${encodeURIComponent(instance)}`;

async function findStudy(study) {
  const { items } = await qido("/studies", { StudyInstanceUID: study });
  if (!items.length) throw new HttpError("This study does not exist (anymore).");
  return items[0];
}

async function findSeries(study, series) {
  const { items } = await qido(`/studies/${encodeURIComponent(study)}/series`, { SeriesInstanceUID: series });
  if (!items.length) throw new HttpError("This series does not exist (anymore).");
  return items[0];
}

const knownNames = { tags: new Map(), uids: new Map() };

/** Keywords of attributes and names of SOP classes, from the server's dictionary. */
async function lookupNames({ tags = [], uids = [] }) {
  const missing = (kind, list) => [...new Set(list)].filter((v) => !knownNames[kind].has(v));
  const wanted = { tags: missing("tags", tags), uids: missing("uids", uids) };
  // Batches keep URLs short.
  const batches = [];
  for (const kind of ["tags", "uids"]) {
    for (let i = 0; i < wanted[kind].length; i += 200) {
      batches.push({ [kind]: wanted[kind].slice(i, i + 200).join(",") });
    }
  }
  await Promise.all(batches.map(async (batch) => {
    try {
      const response = await get(`api/names?${new URLSearchParams(batch)}`, "application/json");
      const found = await response.json();
      for (const kind of ["tags", "uids"]) {
        for (const value of (batch[kind] || "").split(",").filter(Boolean)) {
          knownNames[kind].set(value, found[kind][value] || "");
        }
      }
    } catch {
      // Names are a convenience: show tags and UIDs alone.
    }
  }));
}

async function refreshStatistics() {
  const stats = document.getElementById("stats");
  try {
    const response = await fetch("../admin/statistics");
    if (!response.ok) return;
    const s = await response.json();
    stats.replaceChildren(
      h("span", {}, h("b", {}, integer.format(s.studies)), " studies"),
      h("span", {}, h("b", {}, integer.format(s.series)), " series"),
      h("span", {}, h("b", {}, integer.format(s.instances)), " instances"),
      h("span", {}, h("b", {}, formatSize(s.size))));
  } catch {
    stats.replaceChildren();
  }
}

// ---------------------------------------------------------------------------------------------
// Tables

function compare(a, b) {
  const empty = (v) => v == null || v === "";
  if (empty(a) || empty(b)) return empty(a) - empty(b);
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: "base" });
}

/**
 * A table whose rows open `href(row)` and whose columns sort on click.
 * Columns: `{ label, value(row), render?(row), num?, wrap?, sort? }`.
 */
function dataTable(columns, { href, sort, empty = "Nothing found." } = {}) {
  let rows = [];
  let order = sort || null; // { column, direction }
  const heads = columns.map((column, i) => {
    const sortable = column.sort !== false;
    return h("th", {
      class: [sortable && "sortable", column.num && "num"].filter(Boolean).join(" ") || null,
      onclick: sortable ? () => {
        order = { column: i, direction: order && order.column === i ? -order.direction : 1 };
        draw();
      } : null,
    }, column.label);
  });
  const body = h("tbody");
  const el = h("div", { class: "table-wrap" }, h("table", {}, h("thead", {}, h("tr", {}, heads)), body));

  function open(row, event) {
    const target = href(row);
    if (event && (event.ctrlKey || event.metaKey || event.button === 1)) window.open(target, "_blank");
    else location.hash = target;
  }

  function draw() {
    heads.forEach((th, i) => {
      th.querySelector(".arrow")?.remove();
      if (order && order.column === i) th.append(h("span", { class: "arrow" }, order.direction > 0 ? "↑" : "↓"));
    });
    let list = rows;
    if (order) {
      const value = columns[order.column].value;
      list = [...rows].sort((a, b) => compare(value(a), value(b)) * order.direction);
    }
    if (!list.length) {
      body.replaceChildren(h("tr", {}, h("td", { colspan: columns.length, class: "empty" }, empty)));
      return;
    }
    body.replaceChildren(...list.map((row) => h("tr", {
      class: href ? "link" : null,
      tabindex: href ? 0 : null,
      onclick: href ? (e) => !e.target.closest("a, button") && open(row, e) : null,
      onauxclick: href ? (e) => e.button === 1 && !e.target.closest("a, button") && open(row, e) : null,
      onkeydown: href ? (e) => e.key === "Enter" && open(row, e) : null,
    }, columns.map((column) => h("td", {
      class: [column.num && "num", column.wrap && "wrap", column.cls].filter(Boolean).join(" ") || null,
    }, column.render ? column.render(row) : column.value(row))))));
  }

  draw();
  return {
    el,
    get rows() { return rows; },
    set(list) { rows = list; draw(); },
    add(list) { rows = rows.concat(list); draw(); },
  };
}

/**
 * A card with a title, a count and a table filled by pages of `fetchPage(offset)`, which
 * resolves to `{ items, truncated, warning? }`.
 */
function resultsCard(title, table, fetchPage, { pageSize = PAGE } = {}) {
  const count = h("span", { class: "count" });
  const messages = h("div");
  const more = h("button", { onclick: () => load() }, "Load more");
  const footer = h("div", { class: "more", hidden: true }, more);
  const content = h("div", {}, loading());
  const card = h("section", { class: "card" },
    h("div", { class: "card-head" }, h("h2", {}, title, count)), messages, content, footer);

  async function load() {
    more.disabled = true;
    try {
      const { items, truncated, warning } = await fetchPage(table.rows.length);
      if (content.firstChild !== table.el) content.replaceChildren(table.el);
      if (warning) messages.replaceChildren(notice("warn", warning));
      table.add(items);
      const full = items.length >= pageSize || truncated;
      footer.hidden = !full;
      count.textContent = `${integer.format(table.rows.length)}${full ? "+" : ""}`;
    } catch (e) {
      if (e.name === "AbortError") return;
      messages.replaceChildren(notice("error", e.message));
      if (!table.rows.length) content.replaceChildren();
    } finally {
      more.disabled = false;
    }
  }

  load();
  return card;
}

// Column definitions shared by several views.

const patientLink = (item) => h("a", { href: patientHref(item) }, name(item) || "(no name)");

const columns = {
  patientName: { label: "Patient", value: name, render: patientLink },
  patientId: { label: "Patient ID", value: (r) => text(r, "PatientID") },
  birthDate: { label: "Birth date", value: (r) => formatDate(text(r, "PatientBirthDate")) },
  studyDate: { label: "Study date", value: (r) => dateTime(r, "StudyDate", "StudyTime") },
  studyDescription: { label: "Description", value: (r) => text(r, "StudyDescription"), wrap: true },
  accession: { label: "Accession", value: (r) => text(r, "AccessionNumber") },
  modalities: {
    label: "Modalities",
    value: (r) => values(r, "ModalitiesInStudy").join(" "),
    render: (r) => badges(values(r, "ModalitiesInStudy")),
  },
  studySeries: { label: "Series", value: (r) => number(r, "NumberOfStudyRelatedSeries"), num: true },
  studyInstances: { label: "Instances", value: (r) => number(r, "NumberOfStudyRelatedInstances"), num: true },
  seriesNumber: { label: "#", value: (r) => number(r, "SeriesNumber"), num: true },
  modality: { label: "Modality", value: (r) => text(r, "Modality"), render: (r) => badges([text(r, "Modality")]) },
  seriesDescription: { label: "Series description", value: (r) => text(r, "SeriesDescription"), wrap: true },
  seriesInstances: { label: "Instances", value: (r) => number(r, "NumberOfSeriesRelatedInstances"), num: true },
};

const studyRoute = (study) => `#/studies/${encodeURIComponent(study)}`;
const seriesRoute = (study, series) => `${studyRoute(study)}/series/${encodeURIComponent(series)}`;
const instanceRoute = (study, series, instance) => `${seriesRoute(study, series)}/instances/${encodeURIComponent(instance)}`;
// For search results, which carry the UIDs of the upper levels.
const studyHref = (item) => studyRoute(text(item, "StudyInstanceUID"));
const seriesHref = (item) => seriesRoute(text(item, "StudyInstanceUID"), text(item, "SeriesInstanceUID"));

function patientHref(item) {
  const params = new URLSearchParams({ id: text(item, "PatientID") });
  if (text(item, "IssuerOfPatientID")) params.set("issuer", text(item, "IssuerOfPatientID"));
  if (!text(item, "PatientID")) params.set("name", text(item, "PatientName"));
  return `#/patient?${params}`;
}

const byDateDesc = (columnIndex) => ({ column: columnIndex, direction: -1 });

// ---------------------------------------------------------------------------------------------
// Search

const FIELDS = [
  { key: "PatientID", label: "Patient ID", match: "prefix" },
  { key: "PatientName", label: "Patient name", match: "contains", placeholder: "Doe" },
  { key: "AccessionNumber", label: "Accession number", match: "prefix" },
  { key: "StudyDescription", label: "Study description", match: "contains", placeholder: "case-sensitive" },
];

const LEVELS = [
  ["patients", "Patients"],
  ["studies", "Studies"],
  ["series", "Series"],
];

/** The last search, for the breadcrumbs of the views opened from it. */
let lastSearch = "#/";

/** A QIDO-RS matching value: as typed with wildcards, else a prefix or substring match. */
function matching(value, match) {
  value = value.trim();
  if (!value) return null;
  if (/[*?]/.test(value)) return value;
  return match === "contains" ? `*${value}*` : `${value}*`;
}

const dicomDate = (value) => (value || "").replaceAll("-", "");
const isoDate = (date) => `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;

/** QIDO-RS parameters of the search form state, with study or series level modality. */
function searchParams(state, level) {
  const params = {};
  for (const field of FIELDS) params[field.key] = matching(state.get(field.key) || "", field.match);
  const from = dicomDate(state.get("from"));
  const to = dicomDate(state.get("to"));
  if (from || to) params.StudyDate = from === to ? from : `${from}-${to}`;
  const modality = (state.get("Modality") || "").trim().toUpperCase();
  if (modality) params[level === "series" ? "Modality" : "ModalitiesInStudy"] = modality;
  return params;
}

/**
 * Inputs of the search criteria (patient, accession, description, modality, study date) set
 * from `state`, their grid, and the date range chips, which call `submit()`.
 */
function criteriaForm(state, submit) {
  const inputs = {};
  const fields = FIELDS.map((field) => h("label", { class: "field" }, field.label,
    inputs[field.key] = h("input", {
      name: field.key,
      value: state.get(field.key) || "",
      placeholder: field.placeholder || "",
      title: field.match === "contains"
        ? "Matches values containing this text; * and ? are wildcards"
        : "Matches values starting with this text; * and ? are wildcards",
      autocomplete: "off",
    })));
  inputs.from = h("input", { type: "date", name: "from", value: state.get("from") || "", "aria-label": "From" });
  inputs.to = h("input", { type: "date", name: "to", value: state.get("to") || "", "aria-label": "To" });
  inputs.Modality = h("input", {
    name: "Modality",
    list: "modalities",
    value: state.get("Modality") || "",
    placeholder: "CT, MR…",
    autocomplete: "off",
  });

  const setRange = (days) => {
    if (days == null) {
      inputs.from.value = inputs.to.value = "";
    } else {
      const today = new Date();
      const from = new Date(today);
      from.setDate(today.getDate() - days);
      inputs.from.value = isoDate(from);
      inputs.to.value = isoDate(today);
    }
    submit();
  };

  const grid = h("div", { class: "search-grid" },
    fields,
    h("label", { class: "field" }, "Modality", inputs.Modality,
      h("datalist", { id: "modalities" }, MODALITIES.map((m) => h("option", { value: m })))),
    h("label", { class: "field wide" }, "Study date",
      h("div", { class: "date-range" }, inputs.from, h("span", { class: "muted" }, "→"), inputs.to)));
  const chips = h("div", { class: "chips" },
    h("button", { type: "button", class: "small", onclick: () => setRange(0) }, "Today"),
    h("button", { type: "button", class: "small", onclick: () => setRange(1) }, "Yesterday"),
    h("button", { type: "button", class: "small", onclick: () => setRange(7) }, "Last 7 days"),
    h("button", { type: "button", class: "small", onclick: () => setRange(30) }, "Last 30 days"),
    h("button", { type: "button", class: "small", onclick: () => setRange(null) }, "Any date"));
  return { inputs, grid, chips };
}

function searchView(view, state) {
  lastSearch = location.hash || "#/";
  setCrumbs([["Search"]]);
  const level = LEVELS.some(([l]) => l === state.get("level")) ? state.get("level") : "studies";

  // Lookup by UID.
  const uid = h("input", {
    name: "uid",
    placeholder: "StudyInstanceUID, SeriesInstanceUID or SOPInstanceUID",
    autocomplete: "off",
    spellcheck: "false",
    "aria-label": "Instance UID",
  });
  const lookupMessage = h("div");
  const lookup = h("section", { class: "card" },
    h("div", { class: "card-head" }, h("h2", {}, "Find by UID")),
    h("form", {
      class: "card-body",
      onsubmit: (e) => {
        e.preventDefault();
        findUid(uid.value.trim(), lookupMessage);
      },
    }, h("div", { class: "lookup" }, uid, h("button", { class: "primary", type: "submit" }, "Open")), lookupMessage));

  // Search form.
  const { inputs, grid, chips } = criteriaForm(state, () => submit());

  let selectedLevel = level;
  const levelButtons = LEVELS.map(([value, label]) => h("button", {
    type: "button",
    "aria-pressed": String(value === level),
    onclick: () => {
      selectedLevel = value;
      submit();
    },
  }, label));

  function submit() {
    const params = new URLSearchParams();
    for (const [key, input] of Object.entries(inputs)) {
      if (input.value.trim()) params.set(key, input.value.trim());
    }
    params.set("level", selectedLevel);
    const target = `#/?${params}`;
    if (location.hash === target) route();
    else location.hash = target;
  }

  const form = h("form", {
    class: "card-body",
    onsubmit: (e) => {
      e.preventDefault();
      submit();
    },
  },
  grid,
  h("div", { class: "form-foot" },
    chips,
    h("div", { class: "actions" },
      h("div", { class: "segmented", role: "group", "aria-label": "Search level" }, levelButtons),
      h("button", {
        type: "button",
        class: "ghost",
        onclick: () => {
          for (const input of Object.values(inputs)) input.value = "";
          submit();
        },
      }, "Clear"),
      h("button", { class: "primary", type: "submit" }, "Search"))));

  const search = h("section", { class: "card" },
    h("div", { class: "card-head" }, h("h2", {}, "Search")), form);

  view.append(lookup, search, results(level, state));
}

function results(level, state) {
  const params = searchParams(state, level);
  if (level === "patients") return patientResults(params);
  if (level === "series") {
    const table = dataTable([
      columns.patientName,
      columns.patientId,
      columns.studyDate,
      { ...columns.studyDescription, label: "Study description" },
      columns.modality,
      columns.seriesNumber,
      columns.seriesDescription,
      columns.seriesInstances,
    ], { href: seriesHref, sort: byDateDesc(2), empty: "No series match these criteria." });
    return resultsCard("Series", table, (offset) => qido("/series", {
      ...params, includefield: SERIES_INCLUDE.join(","), limit: PAGE, offset,
    }));
  }
  const table = dataTable([
    columns.patientName,
    columns.patientId,
    columns.birthDate,
    columns.studyDate,
    columns.studyDescription,
    columns.accession,
    columns.modalities,
    columns.studySeries,
    columns.studyInstances,
  ], { href: studyHref, sort: byDateDesc(3), empty: "No studies match these criteria." });
  return resultsCard("Studies", table, (offset) => qido("/studies", { ...params, limit: PAGE, offset }));
}

/** Patients, grouped from the matching studies. */
function patientResults(params) {
  const table = dataTable([
    columns.patientName,
    columns.patientId,
    { label: "Issuer", value: (p) => text(p, "IssuerOfPatientID") },
    columns.birthDate,
    { label: "Sex", value: (p) => text(p, "PatientSex") },
    { label: "Studies", value: (p) => p.studies, num: true },
    { label: "Last study", value: (p) => formatDate(p.lastStudy) },
    {
      label: "Modalities",
      value: (p) => [...p.modalities].join(" "),
      render: (p) => badges([...p.modalities].sort()),
    },
  ], { href: patientHref, sort: { column: 0, direction: 1 }, empty: "No patients match these criteria." });

  let fetched = false;
  return resultsCard("Patients", table, async () => {
    if (fetched) return { items: [], truncated: false };
    fetched = true;
    const { items, truncated } = await qido("/studies", { ...params, limit: PATIENT_STUDIES });
    const patients = new Map();
    for (const study of items) {
      const key = `${text(study, "PatientID")}\u0000${text(study, "IssuerOfPatientID")}`;
      let patient = patients.get(key);
      if (!patient) {
        patient = { ...study, studies: 0, lastStudy: "", modalities: new Set() };
        patients.set(key, patient);
      }
      patient.studies++;
      const date = text(study, "StudyDate");
      if (date > patient.lastStudy) patient.lastStudy = date;
      for (const m of values(study, "ModalitiesInStudy")) patient.modalities.add(m);
    }
    const warning = truncated || items.length >= PATIENT_STUDIES
      ? `Only the first ${integer.format(items.length)} matching studies were grouped: refine the search to see every patient.`
      : null;
    // A single page: no "Load more".
    return { items: [...patients.values()], truncated: false, warning };
  }, { pageSize: Infinity });
}

/** Open the study, series or instance with this UID. */
async function findUid(uid, message) {
  message.replaceChildren();
  if (!uid) return;
  if (!/^[0-9.]+$/.test(uid)) {
    message.replaceChildren(notice("error", "A UID is made of digits and dots."));
    return;
  }
  message.replaceChildren(loading("Searching…"));
  try {
    const [studies, series, instances] = await Promise.all([
      qido("/studies", { StudyInstanceUID: uid, limit: 1 }),
      qido("/series", { SeriesInstanceUID: uid, limit: 1 }),
      qido("/instances", { SOPInstanceUID: uid, limit: 1 }),
    ]);
    if (studies.items.length) location.hash = studyHref(studies.items[0]);
    else if (series.items.length) location.hash = seriesHref(series.items[0]);
    else if (instances.items.length) location.hash = `${seriesHref(instances.items[0])}/instances/${encodeURIComponent(uid)}`;
    else {
      message.replaceChildren(notice("warn", "No study, series or instance has this UID."));
    }
  } catch (e) {
    if (e.name !== "AbortError") message.replaceChildren(notice("error", e.message));
  }
}

// ---------------------------------------------------------------------------------------------
// Patient, study, series and instance views

function setCrumbs(items) {
  const crumbs = document.getElementById("crumbs");
  crumbs.replaceChildren(...items.flatMap(([label, href], i) => [
    ...(i ? [h("span", { class: "sep" }, "›")] : []),
    href ? h("a", { href }, label) : h("span", {}, label),
  ]));
}

const crumb = {
  search: () => ["Search", lastSearch],
  patient: (item) => [name(item) || text(item, "PatientID") || "Patient", patientHref(item)],
  study: (item) => [text(item, "StudyDescription") || formatDate(text(item, "StudyDate")) || "Study", studyHref(item)],
  series: (study, item) => [
    `Series ${text(item, "SeriesNumber")} ${text(item, "Modality")}`.replace(/\s+/g, " ").trim(),
    seriesRoute(study, text(item, "SeriesInstanceUID")),
  ],
};

function titleCard(title, subtitle, entries, actions = []) {
  return h("section", { class: "card title-card" },
    h("h1", {}, title),
    subtitle ? h("div", { class: "subtitle" }, subtitle) : null,
    details(entries),
    actions.length ? h("div", { class: "actions", style: "margin-top: 14px" }, actions) : null);
}

/** Link to the instances of a patient, study or series as a ZIP archive, named by the server. */
const archiveLink = (params) =>
  h("a", { class: "button small", href: `api/archive?${new URLSearchParams(params)}`, download: true }, "Download ZIP");

/** Render `build()` into `view` once its data is loaded, or the error. */
async function render(view, build) {
  view.replaceChildren(loading());
  try {
    const nodes = await build();
    view.replaceChildren(...[nodes].flat());
  } catch (e) {
    if (e.name === "AbortError") return;
    view.replaceChildren(notice("error", e.message));
  }
}

function patientView(view, state) {
  const id = state.get("id") || "";
  const issuer = state.get("issuer") || "";
  const params = id ? { PatientID: id } : { PatientName: state.get("name") || "" };
  if (issuer) params.IssuerOfPatientID = issuer;
  setCrumbs([crumb.search(), [id || "Patient"]]);
  if (!id && !params.PatientName) {
    view.append(notice("error", "This patient has neither an ID nor a name: open its studies from a study search."));
    return;
  }

  render(view, async () => {
    const { items, truncated } = await qido("/studies", { ...params, limit: PATIENT_STUDIES });
    if (!items.length) throw new HttpError("No study of this patient was found.");
    const patient = items[0];
    setCrumbs([crumb.search(), [name(patient) || id || "Patient"]]);
    const table = dataTable([
      columns.studyDate,
      columns.studyDescription,
      columns.accession,
      columns.modalities,
      columns.studySeries,
      columns.studyInstances,
    ], { href: studyHref, sort: byDateDesc(0) });
    table.set(items);
    const sending = id ? sendControl("PATIENT", issuer ? { PatientID: id, IssuerOfPatientID: issuer } : { PatientID: id }) : null;
    return [
      titleCard(name(patient) || "(no name)", `Patient ${id}`, [
        ["Patient ID", id, { mono: true }],
        ["Issuer of patient ID", issuer],
        ["Birth date", formatDate(text(patient, "PatientBirthDate"))],
        ["Sex", text(patient, "PatientSex")],
        ["Studies", integer.format(items.length)],
      ], id ? [archiveLink(issuer ? { patient: id, issuer } : { patient: id }), sending.control] : []),
      sending ? sending.messages : null,
      h("section", { class: "card" },
        h("div", { class: "card-head" }, h("h2", {}, "Studies", h("span", { class: "count" }, items.length))),
        truncated ? notice("warn", "This patient has more studies than the server returns at once.") : null,
        table.el),
    ];
  });
}

function studyView(view, state, study) {
  setCrumbs([crumb.search(), ["Study"]]);
  render(view, async () => {
    const [item, series] = await Promise.all([
      findStudy(study),
      qido(`/studies/${encodeURIComponent(study)}/series`),
    ]);
    setCrumbs([crumb.search(), crumb.patient(item), [crumb.study(item)[0]]]);
    const table = dataTable([
      {
        label: "",
        value: () => "",
        sort: false,
        cls: "thumb",
        render: (s) => thumbnail(`${seriesUrl(study, text(s, "SeriesInstanceUID"))}/thumbnail`),
      },
      columns.seriesNumber,
      columns.modality,
      { ...columns.seriesDescription, label: "Description" },
      { label: "Body part", value: (s) => text(s, "BodyPartExamined") },
      { label: "Date", value: (s) => dateTime(s, "SeriesDate", "SeriesTime") },
      columns.seriesInstances,
    ], { href: (s) => seriesRoute(study, text(s, "SeriesInstanceUID")) });
    table.set(series.items);
    const sending = sendControl("STUDY", { StudyInstanceUID: study });
    return [
      titleCard(text(item, "StudyDescription") || "(no description)", dateTime(item, "StudyDate", "StudyTime"), [
        ["Patient", patientLink(item)],
        ["Patient ID", text(item, "PatientID")],
        ["Birth date", formatDate(text(item, "PatientBirthDate"))],
        ["Sex", text(item, "PatientSex")],
        ["Accession number", text(item, "AccessionNumber")],
        ["Study ID", text(item, "StudyID")],
        ["Referring physician", formatName(text(item, "ReferringPhysicianName"))],
        ["Modalities", badges(values(item, "ModalitiesInStudy"))],
        ["Series", integer.format(number(item, "NumberOfStudyRelatedSeries") ?? 0)],
        ["Instances", integer.format(number(item, "NumberOfStudyRelatedInstances") ?? 0)],
        ["Study Instance UID", study, { mono: true }],
      ], [archiveLink({ study }), sending.control]),
      sending.messages,
      h("section", { class: "card" },
        h("div", { class: "card-head" }, h("h2", {}, "Series", h("span", { class: "count" }, series.items.length))),
        table.el),
    ];
  });
}

async function seriesInstances(study, series) {
  const { items, truncated } = await qido(`/studies/${encodeURIComponent(study)}/series/${encodeURIComponent(series)}/instances`);
  return { items, truncated };
}

function seriesView(view, state, study, series) {
  setCrumbs([crumb.search(), ["Series"]]);
  render(view, async () => {
    const [studyItem, item, instances] = await Promise.all([
      findStudy(study),
      findSeries(study, series),
      seriesInstances(study, series),
    ]);
    setCrumbs([crumb.search(), crumb.patient(studyItem), crumb.study(studyItem), [crumb.series(study, item)[0]]]);
    await lookupNames({ uids: instances.items.map((i) => text(i, "SOPClassUID")) });
    const sopClass = (i) => knownNames.uids.get(text(i, "SOPClassUID")) || text(i, "SOPClassUID");
    const table = dataTable([
      {
        label: "",
        value: () => "",
        sort: false,
        cls: "thumb",
        render: (i) => thumbnail(`${instanceUrl(study, series, text(i, "SOPInstanceUID"))}/thumbnail`),
      },
      { label: "#", value: (i) => number(i, "InstanceNumber"), num: true },
      { label: "SOP class", value: sopClass },
      {
        label: "Size",
        value: (i) => (number(i, "Rows") ?? 0) * (number(i, "Columns") ?? 0),
        render: (i) => (number(i, "Rows") ? `${number(i, "Columns")} × ${number(i, "Rows")}` : ""),
        num: true,
      },
      { label: "Frames", value: (i) => number(i, "NumberOfFrames"), num: true },
      { label: "SOP Instance UID", value: (i) => text(i, "SOPInstanceUID"), cls: "mono" },
    ], { href: (i) => instanceRoute(study, series, text(i, "SOPInstanceUID")) });
    table.set(instances.items);
    const title = [text(item, "SeriesNumber") && `#${text(item, "SeriesNumber")}`, text(item, "SeriesDescription") || "(no description)"]
      .filter(Boolean).join(" · ");
    const sending = sendControl("SERIES", { StudyInstanceUID: study, SeriesInstanceUID: series });

    // The instances as a list, or their images one slice at a time. The mode and the slice are
    // kept in the URL without a navigation, so that a reload or a shared link opens them again.
    const slices = seriesSlices(instances.items);
    const signal = navigation.signal;
    const content = h("div");
    let mode = slices.length && state.get("view") === "viewer" ? "viewer" : "list";
    let slice = Math.min(slices.length, Math.max(1, Number(state.get("slice")) || 1)) - 1;
    let opened = null;
    let urlTimer = null;
    const keepUrl = () => {
      clearTimeout(urlTimer);
      urlTimer = setTimeout(() => {
        if (signal.aborted) return;
        const params = mode === "viewer" ? `?${new URLSearchParams({ view: "viewer", slice: slice + 1 })}` : "";
        try { history.replaceState(history.state, "", `${seriesRoute(study, series)}${params}`); } catch { /* not kept */ }
      }, 300);
    };
    const toggle = () => h("div", { class: "segmented", role: "group", "aria-label": "Display" },
      [["list", "List"], ["viewer", "Viewer"]].map(([value, label]) => h("button", {
        type: "button",
        "aria-pressed": String(value === mode),
        onclick: () => show(value),
      }, label)));
    const list = (head) => h("section", { class: "card" },
      h("div", { class: "card-head" },
        h("h2", {}, "Instances", h("span", { class: "count" }, instances.items.length)), head),
      instances.truncated ? notice("warn", "This series has more instances than the server returns at once.") : null,
      table.el);
    const show = (next) => {
      mode = next;
      opened?.dispose();
      opened = null;
      if (mode === "viewer") {
        opened = seriesViewer(study, series, slices, slice, toggle(), signal, (i) => {
          slice = i;
          keepUrl();
        });
        content.replaceChildren(...[
          instances.truncated && notice("warn", "This series has more instances than the server returns at once: the viewer shows the first ones."),
          opened.el,
        ].filter(Boolean));
      } else {
        content.replaceChildren(list(slices.length ? toggle() : null));
      }
      keepUrl();
    };
    show(mode);

    return [
      titleCard(title, `${name(studyItem)} · ${text(studyItem, "StudyDescription") || formatDate(text(studyItem, "StudyDate"))}`, [
        ["Modality", badges([text(item, "Modality")])],
        ["Series number", text(item, "SeriesNumber")],
        ["Body part", text(item, "BodyPartExamined")],
        ["Date", dateTime(item, "SeriesDate", "SeriesTime")],
        ["Instances", integer.format(number(item, "NumberOfSeriesRelatedInstances") ?? instances.items.length)],
        ["Series Instance UID", series, { mono: true }],
      ], [archiveLink({ study, series }), sending.control]),
      sending.messages,
      content,
    ];
  });
}

/** The images of a series, one slice per frame, in the order of its instances (Instance Number). */
function seriesSlices(instances) {
  return instances.filter((i) => number(i, "Rows")).flatMap((instance) => {
    const frames = Math.max(1, number(instance, "NumberOfFrames") || 1);
    return Array.from({ length: frames }, (_, f) => ({ instance, frame: f + 1, frames }));
  });
}

const renderedUrl = (study, series, { instance, frame, frames }) => {
  const base = instanceUrl(study, series, text(instance, "SOPInstanceUID"));
  return frames > 1 ? `${base}/frames/${frame}/rendered` : `${base}/rendered`;
};

/** Slices fetched on each side of the one shown, so that scrolling does not wait for the server. */
const SLICE_WINDOW = 16;
/** Rendered slices fetched at once. */
const SLICE_CONCURRENCY = 4;
/** Rendered slices kept in memory: the farthest from the one shown are dropped first. */
const SLICE_CACHE = 400;

/** Fetches rendered slices, nearest to the current one first, as object URLs. */
class SliceLoader {
  constructor(urls, onLoad) {
    this.urls = urls;
    this.onLoad = onLoad;
    /** Slice index: `{ src }`, or `{ error: true }` when it cannot be rendered. */
    this.loaded = new Map();
    /** Slice index: AbortController of its request. */
    this.running = new Map();
    this.current = 0;
    this.disposed = false;
  }

  focus(index) {
    this.current = index;
    // Requests for slices scrolled past make room for the ones now needed.
    for (const [i, request] of this.running) {
      if (Math.abs(i - index) > SLICE_WINDOW) request.abort();
    }
    this.pump();
  }

  next() {
    for (let d = 0; d <= SLICE_WINDOW; d++) {
      for (const i of d ? [this.current + d, this.current - d] : [this.current]) {
        if (i >= 0 && i < this.urls.length && !this.loaded.has(i) && !this.running.has(i)) return i;
      }
    }
    return null;
  }

  pump() {
    while (!this.disposed && this.running.size < SLICE_CONCURRENCY) {
      const i = this.next();
      if (i == null) return;
      this.fetch(i);
    }
  }

  async fetch(i) {
    const request = new AbortController();
    this.running.set(i, request);
    try {
      const response = await get(this.urls[i], "image/jpeg", request.signal);
      const blob = await response.blob();
      if (this.disposed) return;
      this.loaded.set(i, { src: URL.createObjectURL(blob) });
      this.evict();
    } catch (e) {
      if (e.name !== "AbortError" && !this.disposed) this.loaded.set(i, { error: true });
    } finally {
      this.running.delete(i);
      if (!this.disposed) {
        if (this.loaded.has(i)) this.onLoad(i);
        this.pump();
      }
    }
  }

  evict() {
    if (this.loaded.size <= SLICE_CACHE) return;
    const farthest = [...this.loaded.keys()].sort((a, b) => Math.abs(b - this.current) - Math.abs(a - this.current));
    for (const i of farthest.slice(0, this.loaded.size - SLICE_CACHE)) {
      if (Math.abs(i - this.current) <= SLICE_WINDOW) break;
      const { src } = this.loaded.get(i);
      if (src) URL.revokeObjectURL(src);
      this.loaded.delete(i);
    }
  }

  dispose() {
    this.disposed = true;
    for (const request of this.running.values()) request.abort();
    for (const { src } of this.loaded.values()) if (src) URL.revokeObjectURL(src);
    this.loaded.clear();
  }
}

/**
 * The instance viewer for a whole series: the mouse wheel over the image, the slider or the
 * arrow keys scroll through the slices, and the tags follow the instance shown.
 */
function seriesViewer(study, series, slices, start, toggle, signal, onSlice) {
  let index = start;
  const loader = new SliceLoader(slices.map((s) => renderedUrl(study, series, s)), (i) => {
    if (i === index) draw();
  });
  const metadata = new Map();

  const stage = h("div", { class: "stage" });
  const unrenderable = () => h("div", { class: "empty" }, "This slice cannot be rendered.");
  const img = h("img", { alt: "Rendered slice" });
  img.addEventListener("error", () => img.isConnected && stage.replaceChildren(unrenderable()));
  const count = h("span", { class: "count" });
  const label = h("span", { class: "muted mono" });
  const slider = slices.length > 1 ? h("input", {
    type: "range", min: 1, max: slices.length, value: index + 1, "aria-label": "Slice",
    oninput: (e) => go(Number(e.target.value) - 1),
  }) : null;
  const info = h("div");
  const tags = h("div", {}, loading());

  function draw() {
    const entry = loader.loaded.get(index);
    if (entry?.src) {
      if (img.getAttribute("src") !== entry.src) img.src = entry.src;
      if (!img.isConnected) stage.replaceChildren(img);
    } else if (entry?.error) {
      stage.replaceChildren(unrenderable());
    }
    // Until it arrives, the previous slice stays on screen.
    stage.classList.toggle("pending", !entry);
  }

  function describe() {
    const { instance, frame, frames } = slices[index];
    const sop = text(instance, "SOPInstanceUID");
    const instanceNumber = text(instance, "InstanceNumber");
    count.textContent = `${index + 1} of ${slices.length}`;
    label.textContent = [instanceNumber && `Instance ${instanceNumber}`, frames > 1 && `frame ${frame} / ${frames}`].filter(Boolean).join(" · ");
    const download = `${WADO}?${new URLSearchParams({
      requestType: "WADO", studyUID: study, seriesUID: series, objectUID: sop, contentType: "application/dicom",
    })}`;
    info.replaceChildren(
      details([
        ["SOP class", knownNames.uids.get(text(instance, "SOPClassUID")) || text(instance, "SOPClassUID")],
        ["Size", text(instance, "Rows") ? `${text(instance, "Columns")} × ${text(instance, "Rows")}` : ""],
        ["Frames", frames > 1 ? String(frames) : ""],
        ["SOP Instance UID", sop, { mono: true }],
      ]),
      h("div", { class: "actions", style: "margin-top: 14px" },
        h("a", { class: "button small", href: instanceRoute(study, series, sop) }, "Open instance"),
        h("a", { class: "button small", href: download, download: `${sop}.dcm` }, "Download DICOM"),
        h("a", { class: "button small", href: renderedUrl(study, series, slices[index]), target: "_blank" }, "Open image")));
  }

  // The tags of the instance shown, once the scrolling pauses.
  let tagsFor = null;
  let tagsTimer = null;
  function scheduleTags() {
    clearTimeout(tagsTimer);
    tagsTimer = setTimeout(async () => {
      const sop = text(slices[index].instance, "SOPInstanceUID");
      if (sop === tagsFor) return;
      try {
        let dataset = metadata.get(sop);
        if (!dataset) {
          const response = await get(`${instanceUrl(study, series, sop)}/metadata`, "application/dicom+json", signal);
          dataset = (await response.json())[0] || {};
          metadata.set(sop, dataset);
          await lookupNames({ tags: collectTags(dataset) });
        }
        if (signal.aborted || sop !== text(slices[index].instance, "SOPInstanceUID")) return;
        tagsFor = sop;
        const query = tags.querySelector(".filter")?.value || "";
        tags.replaceChildren(tagsCard(dataset, query));
      } catch (e) {
        if (e.name !== "AbortError") tags.replaceChildren(notice("error", e.message));
      }
    }, 250);
  }

  function go(target) {
    const next = Math.min(slices.length - 1, Math.max(0, target));
    if (next === index) return;
    index = next;
    if (slider) slider.value = index + 1;
    loader.focus(index);
    draw();
    describe();
    scheduleTags();
    onSlice(index);
  }

  // One slice per notch of a mouse wheel; small trackpad deltas add up.
  let wheel = 0;
  if (slices.length > 1) stage.addEventListener("wheel", (e) => {
    e.preventDefault();
    const pixels = e.deltaY * (e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1);
    if (Math.abs(pixels) >= 50) {
      wheel = 0;
      go(index + Math.sign(pixels));
      return;
    }
    wheel += pixels;
    if (Math.abs(wheel) >= 30) {
      const steps = Math.trunc(wheel / 30);
      wheel -= steps * 30;
      go(index + steps);
    }
  }, { passive: false });

  // Arrow keys: previous and next slice; page up and down: ten slices; home and end.
  const keys = (e) => {
    if (e.target.closest("input, select, textarea") || e.altKey || e.ctrlKey || e.metaKey) return;
    const steps = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1, PageDown: 10, PageUp: -10 }[e.key];
    if (steps) go(index + steps);
    else if (e.key === "Home") go(0);
    else if (e.key === "End") go(slices.length - 1);
    else return;
    e.preventDefault();
  };
  document.addEventListener("keydown", keys);

  function dispose() {
    document.removeEventListener("keydown", keys);
    clearTimeout(tagsTimer);
    loader.dispose();
  }
  signal.addEventListener("abort", dispose, { once: true });

  loader.focus(index);
  describe();
  scheduleTags();

  const el = h("div", { class: "instance" },
    h("section", { class: "card viewer" },
      h("div", { class: "card-head" }, h("h2", {}, "Slice", count), toggle),
      stage,
      slider ? h("div", { class: "controls" }, slider, label) : h("div", { class: "controls" }, label),
      h("div", { class: "card-body" }, info)),
    tags);
  return { el, dispose };
}

function instanceView(view, state, study, series, instance) {
  setCrumbs([crumb.search(), ["Instance"]]);
  render(view, async () => {
    const [studyItem, seriesItem, instances, metadata] = await Promise.all([
      findStudy(study),
      findSeries(study, series),
      seriesInstances(study, series),
      get(`${instanceUrl(study, series, instance)}/metadata`, "application/dicom+json").then((r) => r.json()),
    ]);
    const dataset = metadata[0] || {};
    const index = instances.items.findIndex((i) => text(i, "SOPInstanceUID") === instance);
    setCrumbs([
      crumb.search(), crumb.patient(studyItem), crumb.study(studyItem), crumb.series(study, seriesItem),
      [`Instance ${text(dataset, "InstanceNumber") || index + 1}`],
    ]);
    await lookupNames({ tags: collectTags(dataset), uids: [text(dataset, "SOPClassUID")] });
    return h("div", { class: "instance" },
      viewer(study, series, instance, dataset, instances.items, index),
      tagsCard(dataset));
  });
}

function viewer(study, series, instance, dataset, siblings, index) {
  const base = instanceUrl(study, series, instance);
  const frames = Math.max(1, Number(text(dataset, "NumberOfFrames")) || 1);
  const hasPixels = Boolean(dataset["7FE00010"] || dataset["7FE00008"] || dataset["7FE00009"]);
  const sibling = (offset) => {
    const target = siblings[index + offset];
    return target && instanceRoute(study, series, text(target, "SOPInstanceUID"));
  };
  const previous = index >= 0 && sibling(-1);
  const next = index >= 0 && sibling(1);

  const stage = h("div", { class: "stage" });
  const frameLabel = h("span", { class: "muted mono" });
  const openImage = h("a", { class: "button small", target: "_blank", hidden: !hasPixels }, "Open image");
  let frame = 1;
  const showFrame = () => {
    frameLabel.textContent = frames > 1 ? `Frame ${frame} / ${frames}` : "";
    const url = frames > 1 ? `${base}/frames/${frame}/rendered` : `${base}/rendered`;
    openImage.href = url;
    const img = h("img", { src: url, alt: "Rendered instance" });
    img.addEventListener("error", () => img.replaceWith(h("div", { class: "empty" }, "This instance cannot be rendered.")));
    stage.replaceChildren(img);
  };
  const slider = frames > 1 ? h("input", {
    type: "range", min: 1, max: frames, value: 1, "aria-label": "Frame",
    oninput: (e) => {
      frame = Number(e.target.value);
      showFrame();
    },
  }) : null;
  if (hasPixels) showFrame();
  else stage.replaceChildren(h("div", { class: "empty" }, "No pixel data"));

  // Arrow keys: previous and next instance; up and down: frames.
  const keys = (e) => {
    if (e.target.closest("input, select, textarea")) return;
    if (e.key === "ArrowLeft" && previous) location.hash = previous;
    else if (e.key === "ArrowRight" && next) location.hash = next;
    else if (slider && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
      e.preventDefault();
      frame = Math.min(frames, Math.max(1, frame + (e.key === "ArrowUp" ? 1 : -1)));
      slider.value = frame;
      showFrame();
    } else return;
  };
  document.addEventListener("keydown", keys);
  navigation.signal.addEventListener("abort", () => document.removeEventListener("keydown", keys));

  const download = `${WADO}?${new URLSearchParams({
    requestType: "WADO", studyUID: study, seriesUID: series, objectUID: instance, contentType: "application/dicom",
  })}`;
  const sopClass = knownNames.uids.get(text(dataset, "SOPClassUID")) || text(dataset, "SOPClassUID");

  return h("section", { class: "card viewer" },
    h("div", { class: "card-head" },
      h("h2", {}, `Instance ${text(dataset, "InstanceNumber") || ""}`,
        h("span", { class: "count" }, index >= 0 ? `${index + 1} of ${siblings.length}` : "")),
      h("div", { class: "actions" },
        h("button", { class: "small", disabled: !previous, title: "Previous (←)", onclick: () => (location.hash = previous) }, "‹ Prev"),
        h("button", { class: "small", disabled: !next, title: "Next (→)", onclick: () => (location.hash = next) }, "Next ›"))),
    stage,
    slider ? h("div", { class: "controls" }, slider, frameLabel) : null,
    h("div", { class: "card-body" },
      details([
        ["SOP class", sopClass],
        ["Size", text(dataset, "Rows") ? `${text(dataset, "Columns")} × ${text(dataset, "Rows")}` : ""],
        ["Frames", frames > 1 ? String(frames) : ""],
        ["SOP Instance UID", instance, { mono: true }],
      ]),
      h("div", { class: "actions", style: "margin-top: 14px" },
        h("a", { class: "button small", href: download, download: `${instance}.dcm` }, "Download DICOM"),
        openImage)));
}

/** Tags of a DICOM JSON dataset and of its sequence items. */
function collectTags(dataset, into = []) {
  for (const [tag, element] of Object.entries(dataset)) {
    into.push(tag);
    if (element.vr === "SQ") for (const item of element.Value || []) collectTags(item, into);
  }
  return into;
}

function formatValue(element) {
  if (element.BulkDataURI) return h("a", { href: element.BulkDataURI, target: "_blank" }, "bulk data");
  if (element.InlineBinary != null) {
    const bytes = Math.floor((element.InlineBinary.length * 3) / 4);
    return h("span", { class: "muted" }, `binary, ~${integer.format(bytes)} bytes`);
  }
  const list = (element.Value || []).map((v) => {
    if (v && typeof v === "object") return element.vr === "PN" ? v.Alphabetic || "" : JSON.stringify(v);
    return String(v);
  });
  if (element.vr === "UI") {
    const known = list.map((v) => knownNames.uids.get(v)).filter(Boolean);
    if (known.length) return [list.join("\\"), " ", h("span", { class: "muted" }, `(${known.join(", ")})`)];
  }
  const joined = list.join("\\");
  return joined.length > 600 ? `${joined.slice(0, 600)}… (${integer.format(list.length)} values)` : joined;
}

function tagsCard(dataset, query = "") {
  const filter = h("input", { class: "filter", type: "search", placeholder: "Filter tags…", "aria-label": "Filter tags" });
  filter.value = query;
  const body = h("tbody");

  function rows(items, depth, query) {
    const out = [];
    for (const [tag, element] of Object.entries(items).sort(([a], [b]) => a.localeCompare(b))) {
      const keyword = knownNames.tags.get(tag) || "";
      const label = `(${tag.slice(0, 4)},${tag.slice(4)})`;
      let nested = [];
      if (element.vr === "SQ") {
        (element.Value || []).forEach((item, i) => {
          const inner = rows(item, depth + 1, query);
          if (inner.length || !query) {
            nested.push(h("tr", { class: "nested item-row", style: `--depth: ${depth + 1}` },
              h("td", { colspan: 4 }, `Item ${i + 1}`)), ...inner);
          }
        });
      }
      const value = element.vr === "SQ" ? h("span", { class: "muted" }, `${(element.Value || []).length} item(s)`) : formatValue(element);
      const matches = !query || [tag, label, keyword, typeof value === "string" ? value : ""].some((v) => v.toLowerCase().includes(query));
      if (matches || nested.length) {
        out.push(h("tr", { class: depth ? "nested" : null, style: `--depth: ${depth}` },
          h("td", { class: "tag" }, label),
          h("td", {}, keyword || h("span", { class: "muted" }, parseInt(tag.slice(0, 4), 16) & 1 ? "private" : "unknown")),
          h("td", { class: "vr" }, element.vr),
          h("td", { class: "value" }, value)));
        out.push(...nested);
      }
    }
    return out;
  }

  const draw = () => {
    const result = rows(dataset, 0, filter.value.trim().toLowerCase());
    body.replaceChildren(...(result.length ? result : [h("tr", {}, h("td", { colspan: 4, class: "empty" }, "No tag matches."))]));
  };
  filter.addEventListener("input", draw);
  draw();

  return h("section", { class: "card" },
    h("div", { class: "card-head" }, h("h2", {}, "Tags", h("span", { class: "count" }, Object.keys(dataset).length)), filter),
    h("div", { class: "table-wrap" }, h("table", { class: "tags" },
      h("thead", {}, h("tr", {}, h("th", {}, "Tag"), h("th", {}, "Name"), h("th", {}, "VR"), h("th", {}, "Value"))),
      body)));
}

// ---------------------------------------------------------------------------------------------
// Upload

/** Files sent at once, one STOW-RS request each, so that every file gets its own outcome. */
const UPLOAD_CONCURRENCY = 4;

const UPLOAD = {
  StudyRetrieveURL: "00081190",
  FailureReason: "00081197",
  WarningReason: "00081196",
  FailedSOPSequence: "00081198",
  ReferencedSOPSequence: "00081199",
  OtherFailuresSequence: "0008119A",
};

const FAILURE_REASONS = {
  0x0110: "Processing failure",
  0xa700: "Out of resources",
  0xa900: "Invalid or conflicting attributes",
  0xc000: "Not a readable DICOM file",
};

/**
 * The upload in progress, kept across views: it goes on while the user browses, and the
 * upload view shows it again when reopened.
 */
class Upload {
  constructor() {
    this.queue = [];
    this.files = 0;
    this.bytes = 0;
    this.doneFiles = 0;
    this.doneBytes = 0;
    this.stored = 0;
    this.duplicates = 0;
    this.skipped = 0;
    this.unsent = 0;
    /** Why the server refused the upload as a whole (credentials). */
    this.refused = null;
    this.failed = []; // { name, reason }
    this.studies = new Set();
    this.studyItems = null;
    this.running = false;
    this.cancelled = false;
    this.controller = new AbortController();
    this.listeners = new Set();
    this.scheduled = false;
  }

  get finished() {
    return !this.running && this.files > 0;
  }

  add(files) {
    for (const file of files) {
      // DICOMDIR indexes the other files and holds no instance; hidden files are OS clutter.
      if (file.name === "DICOMDIR" || file.name.startsWith(".")) {
        this.skipped++;
        continue;
      }
      this.queue.push(file);
      this.files++;
      this.bytes += file.size;
    }
    this.changed();
    if (!this.running && this.queue.length) this.run();
  }

  cancel() {
    this.cancelled = true;
    this.queue = [];
    this.controller.abort();
    this.changed();
  }

  async run() {
    this.running = true;
    this.studyItems = null;
    this.changed();
    await Promise.all(Array.from({ length: UPLOAD_CONCURRENCY }, () => this.worker()));
    this.running = false;
    this.changed();
    refreshStatistics();
    if (this.studies.size) {
      try {
        this.studyItems = await uploadedStudies([...this.studies]);
      } catch {
        this.studyItems = [];
      }
      this.changed();
    }
  }

  async worker() {
    while (this.queue.length) {
      const file = this.queue.shift();
      try {
        const outcome = await stowFile(file, this.controller.signal);
        if (outcome.study) this.studies.add(outcome.study);
        if (outcome.failure) this.failed.push({ name: fileName(file), reason: outcome.failure });
        else if (outcome.duplicate) this.duplicates++;
        else this.stored++;
      } catch (e) {
        if (e.name === "AbortError") return;
        if (e.status === 401 || e.status === 403) {
          // Credentials apply to every file: do not send the others.
          this.refused = this.refused || e.message;
          this.unsent += 1 + this.queue.length;
          this.doneFiles += this.queue.length;
          this.doneBytes += this.queue.reduce((sum, f) => sum + f.size, 0);
          this.queue = [];
        } else {
          this.failed.push({ name: fileName(file), reason: e.message });
        }
      }
      this.doneFiles++;
      this.doneBytes += file.size;
      this.changed();
    }
  }

  /** Listeners are called at most once per animation frame. */
  changed() {
    if (this.scheduled) return;
    this.scheduled = true;
    requestAnimationFrame(() => {
      this.scheduled = false;
      for (const listener of this.listeners) listener();
    });
  }
}

let upload = new Upload();

window.addEventListener("beforeunload", (e) => {
  if (upload.running) e.preventDefault();
});

const fileName = (file) => file.webkitRelativePath || file.relativePath || file.name;

/** Store one file with STOW-RS; resolves to `{ study?, duplicate?, failure? }`. */
async function stowFile(file, signal) {
  const boundary = `fortitude-${crypto.getRandomValues(new Uint32Array(2)).join("")}`;
  const body = new Blob([
    `--${boundary}\r\nContent-Type: application/dicom\r\n\r\n`,
    file,
    `\r\n--${boundary}--\r\n`,
  ]);
  const response = await fetch(`${ROOT}/studies`, {
    method: "POST",
    headers: {
      Accept: "application/dicom+json",
      "Content-Type": `multipart/related; type="application/dicom"; boundary=${boundary}`,
    },
    body,
    signal,
  });
  const reply = await response.text();
  let result = null;
  if ((response.headers.get("Content-Type") || "").includes("json")) {
    try {
      result = JSON.parse(reply);
    } catch {
      // Reported below as an HTTP error.
    }
  }
  if (!result) {
    const error = new HttpError(`${response.status} ${response.statusText}${reply.trim() ? `: ${reply.trim()}` : ""}`);
    error.status = response.status;
    throw error;
  }
  const referenced = values(result, UPLOAD.ReferencedSOPSequence)[0];
  if (referenced) {
    const url = text(referenced, UPLOAD.StudyRetrieveURL) || text(result, UPLOAD.StudyRetrieveURL);
    const study = /\/studies\/([^/]+)/.exec(url);
    return {
      study: study && decodeURIComponent(study[1]),
      duplicate: number(referenced, UPLOAD.WarningReason) === 0xb111,
    };
  }
  const failed = values(result, UPLOAD.FailedSOPSequence)[0] || values(result, UPLOAD.OtherFailuresSequence)[0];
  const reason = number(failed, UPLOAD.FailureReason);
  return {
    failure: FAILURE_REASONS[reason] || (reason != null ? `Failure 0x${reason.toString(16).toUpperCase()}` : `${response.status} ${response.statusText}`),
  };
}

/** QIDO-RS results of these studies, by groups that keep URLs short. */
async function uploadedStudies(uids) {
  const items = [];
  for (let i = 0; i < uids.length; i += 50) {
    const { items: found } = await qido("/studies", { StudyInstanceUID: uids.slice(i, i + 50).join(",") }, null);
    items.push(...found);
  }
  return items;
}

/** Files of a drop, folders included (read recursively). */
async function droppedFiles(dataTransfer) {
  const entries = [...dataTransfer.items]
    .map((item) => item.webkitGetAsEntry && item.webkitGetAsEntry())
    .filter(Boolean);
  if (!entries.length) return [...dataTransfer.files];
  const files = [];
  const visit = async (entry, path) => {
    if (entry.isFile) {
      const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
      file.relativePath = `${path}${file.name}`;
      files.push(file);
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      // readEntries returns the entries of a directory in batches, until an empty one.
      for (;;) {
        const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
        if (!batch.length) break;
        for (const child of batch) await visit(child, `${path}${entry.name}/`);
      }
    }
  };
  for (const entry of entries) await visit(entry, "");
  return files;
}

function uploadView(view) {
  setCrumbs([crumb.search(), ["Upload"]]);

  const filesInput = h("input", { type: "file", multiple: true, hidden: true });
  const folderInput = h("input", { type: "file", webkitdirectory: true, hidden: true });
  for (const input of [filesInput, folderInput]) {
    input.addEventListener("change", () => {
      start([...input.files]);
      input.value = "";
    });
  }

  const start = (files) => {
    if (!files.length) return;
    if (!upload.running) {
      const listeners = upload.listeners;
      upload = new Upload();
      upload.listeners = listeners;
    }
    upload.add(files);
  };

  const zone = h("div", { class: "dropzone" },
    h("div", { class: "dropzone-title" }, "Drop DICOM files or folders here"),
    h("div", { class: "muted" }, "Each file is stored with STOW-RS; files already stored are reported, not duplicated."),
    h("div", { class: "actions", style: "justify-content: center; margin-top: 14px" },
      h("button", { class: "primary", onclick: () => filesInput.click() }, "Choose files"),
      h("button", { onclick: () => folderInput.click() }, "Choose a folder")),
    filesInput, folderInput);

  let depth = 0;
  zone.addEventListener("dragenter", (e) => {
    e.preventDefault();
    depth++;
    zone.classList.add("over");
  });
  zone.addEventListener("dragleave", () => {
    if (--depth <= 0) {
      depth = 0;
      zone.classList.remove("over");
    }
  });
  zone.addEventListener("dragover", (e) => e.preventDefault());
  zone.addEventListener("drop", async (e) => {
    e.preventDefault();
    depth = 0;
    zone.classList.remove("over");
    start(await droppedFiles(e.dataTransfer));
  });
  // A drop beside the zone must not make the browser open the file.
  const ignore = (e) => !e.target.closest(".dropzone") && e.preventDefault();
  for (const type of ["dragover", "drop"]) {
    document.addEventListener(type, ignore);
    navigation.signal.addEventListener("abort", () => document.removeEventListener(type, ignore));
  }

  const status = h("div");
  const draw = () => status.replaceChildren(...uploadStatus());
  upload.listeners.add(draw);
  navigation.signal.addEventListener("abort", () => upload.listeners.delete(draw));
  draw();

  view.append(
    h("section", { class: "card" }, h("div", { class: "card-head" }, h("h2", {}, "Upload")), h("div", { class: "card-body" }, zone)),
    status);
}

function uploadStatus() {
  const u = upload;
  if (!u.files && !u.skipped) return [];
  const percent = u.bytes ? Math.round((u.doneBytes / u.bytes) * 100) : 100;
  const state = u.running ? "Uploading…" : u.refused ? "Refused" : u.cancelled ? "Cancelled" : "Done";
  const tally = (label, value, kind) => h("div", { class: `tally ${kind || ""}` }, h("b", {}, integer.format(value)), " ", label);
  const nodes = [h("section", { class: "card" },
    h("div", { class: "card-head" },
      h("h2", {}, state, h("span", { class: "count" },
        `${integer.format(u.doneFiles)} / ${integer.format(u.files)} files · ${formatSize(u.doneBytes)} / ${formatSize(u.bytes)}`)),
      u.running ? h("button", { class: "small", onclick: () => u.cancel() }, "Cancel") : null),
    h("div", { class: "card-body" },
      u.refused ? h("div", { style: "margin-bottom: 14px" }, notice("error", `The server refused the upload: ${u.refused}`)) : null,
      h("div", { class: "progress", role: "progressbar", "aria-valuenow": percent, "aria-valuemin": 0, "aria-valuemax": 100 },
        h("div", { class: "bar", style: `width: ${percent}%` })),
      h("div", { class: "tallies" },
        tally("stored", u.stored, "ok"),
        tally("already stored", u.duplicates),
        tally("failed", u.failed.length, u.failed.length ? "bad" : ""),
        u.unsent ? tally("not sent", u.unsent, "bad") : null,
        u.skipped ? tally("skipped (DICOMDIR, hidden)", u.skipped) : null)),
    u.failed.length ? h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "File"), h("th", {}, "Reason"))),
      h("tbody", {}, u.failed.slice(0, 200).map((f) => h("tr", {}, h("td", { class: "wrap mono" }, f.name), h("td", { class: "wrap" }, f.reason))),
        u.failed.length > 200 ? h("tr", {}, h("td", { colspan: 2, class: "muted" }, `and ${integer.format(u.failed.length - 200)} more`)) : null))) : null)];

  if (u.finished && u.studies.size) {
    const card = h("section", { class: "card" },
      h("div", { class: "card-head" }, h("h2", {}, "Uploaded studies", h("span", { class: "count" }, u.studies.size))));
    if (!u.studyItems) card.append(loading());
    else {
      const table = dataTable([
        columns.patientName,
        columns.patientId,
        columns.studyDate,
        columns.studyDescription,
        columns.modalities,
        columns.studySeries,
        columns.studyInstances,
      ], { href: studyHref });
      table.set(u.studyItems);
      card.append(table.el);
    }
    nodes.push(card);
  }
  return nodes;
}

// ---------------------------------------------------------------------------------------------
// Modalities: remote application entities the server verifies (C-ECHO), queries (C-FIND),
// retrieves from (C-MOVE) and sends stored instances to (C-STORE).

const ADMIN = "../admin";
/** Matches asked of a modality per query. */
const REMOTE_LIMIT = 500;

/** Send `body` as JSON with `method`; resolves to the JSON response, if any. */
async function send(method, url, body) {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const reply = await response.text();
  if (!response.ok) {
    throw new HttpError(`${response.status} ${response.statusText}${reply.trim() ? `: ${reply.trim()}` : ""}`);
  }
  return reply ? JSON.parse(reply) : null;
}

const modalityUrl = (ae) => `${ADMIN}/modalities/${encodeURIComponent(ae)}`;
const modalityRoute = (ae) => `#/modalities/${encodeURIComponent(ae)}`;
const remoteStudyRoute = (ae, study) => `${modalityRoute(ae)}/studies/${encodeURIComponent(study)}`;

/** `{ ae_title, listening, modalities }`: the server's AE title and the modalities. */
async function loadModalities() {
  return (await get(`${ADMIN}/modalities`, "application/json")).json();
}

/** C-FIND on a modality: `{ matches, truncated }`. */
async function remoteFind(ae, level, params) {
  const query = new URLSearchParams(Object.entries({ level, limit: REMOTE_LIMIT, ...params })
    .filter(([, v]) => v != null && v !== ""));
  return (await get(`${modalityUrl(ae)}/find?${query}`, "application/json")).json();
}

/** Results of the last verification of each modality, kept while browsing. */
const echoes = new Map();

/** A button verifying the connection with `ae`, and its last result; failures go to `report`. */
function echoControl(ae, report) {
  const result = h("span");
  const show = () => {
    const echo = echoes.get(ae);
    result.className = echo ? `echo-result ${echo.ok === true ? "ok" : echo.ok === false ? "bad" : "muted"}` : "echo-result";
    result.textContent = echo ? echo.text : "";
    result.title = echo && echo.error ? echo.error : "";
  };
  const button = h("button", {
    class: "small",
    title: "Verify the connection (C-ECHO)",
    onclick: async (e) => {
      e.stopPropagation();
      button.disabled = true;
      echoes.set(ae, { ok: null, text: "…" });
      show();
      try {
        const reply = await (await get(`${modalityUrl(ae)}/echo`, "application/json", null)).json();
        echoes.set(ae, { ok: true, text: `✓ ${integer.format(reply.milliseconds)} ms` });
      } catch (error) {
        echoes.set(ae, { ok: false, text: "✗ Failed", error: error.message });
        report(notice("error", `${ae}: ${error.message}`));
      }
      button.disabled = false;
      show();
    },
  }, "Echo");
  show();
  return h("span", { class: "echo" }, button, result);
}

function modalitiesView(view) {
  setCrumbs([["Modalities"]]);
  const messages = h("div");
  const report = (node) => messages.replaceChildren(node);
  const count = h("span", { class: "count" });
  const own = h("div", { class: "card-body muted" });
  const content = h("div", {}, loading());
  const form = modalityForm(() => draw());

  const table = dataTable([
    { label: "AE title", value: (m) => m.ae_title, render: (m) => h("a", { class: "mono", href: modalityRoute(m.ae_title) }, m.ae_title) },
    { label: "Host", value: (m) => m.host, cls: "mono" },
    { label: "Port", value: (m) => m.port, num: true },
    { label: "Description", value: (m) => m.description, wrap: true },
    {
      label: "Source",
      value: (m) => m.source,
      render: (m) => h("span", {
        class: "badge",
        title: m.source === "config" ? "Set in the configuration file (dimse.peers)" : "Registered from this page",
      }, m.source === "config" ? "file" : "registered"),
    },
    { label: "Connection", value: () => "", sort: false, render: (m) => echoControl(m.ae_title, report) },
    {
      label: "",
      value: () => "",
      sort: false,
      render: (m) => h("div", { class: "actions nowrap" },
        h("a", { class: "button small", href: modalityRoute(m.ae_title) }, "Query"),
        m.source === "registered" ? [
          h("button", { class: "small", onclick: () => form.edit(m) }, "Edit"),
          h("button", {
            class: "small ghost",
            onclick: async () => {
              if (!confirm(`Remove the modality ${m.ae_title}?`)) return;
              try {
                await send("DELETE", modalityUrl(m.ae_title));
                echoes.delete(m.ae_title);
                draw();
              } catch (error) {
                report(notice("error", error.message));
              }
            },
          }, "Remove"),
        ] : null),
    },
  ], { href: (m) => modalityRoute(m.ae_title), sort: { column: 0, direction: 1 }, empty: "No modality yet: add one below." });

  async function draw() {
    try {
      const data = await loadModalities();
      table.set(data.modalities);
      count.textContent = data.modalities.length;
      content.replaceChildren(table.el);
      own.replaceChildren(
        "Requests are sent with the AE title ", h("b", { class: "mono" }, data.ae_title), ". ",
        data.listening
          ? "Modalities retrieve to it once they know this server under that AE title (its DIMSE address and port)."
          : h("span", { class: "warn-text" }, "The DIMSE service of this server is disabled (dimse.listen): modalities can be queried, but cannot send it instances."));
    } catch (error) {
      if (error.name !== "AbortError") content.replaceChildren(notice("error", error.message));
    }
  }
  draw();

  view.append(
    h("section", { class: "card" },
      h("div", { class: "card-head" }, h("h2", {}, "Modalities", count)),
      own, messages, content),
    form.el,
    transfersCard("moves"),
    transfersCard("sends"));
}

/** Form registering a modality, or changing a registered one; `saved()` is called after. */
function modalityForm(saved) {
  const inputs = {
    ae: h("input", { name: "ae", required: true, maxlength: 16, placeholder: "PACS", autocomplete: "off", spellcheck: "false" }),
    host: h("input", { name: "host", required: true, placeholder: "10.0.0.5 or pacs.example.org", autocomplete: "off", spellcheck: "false" }),
    port: h("input", { name: "port", type: "number", required: true, min: 1, max: 65535, value: 104 }),
    description: h("input", { name: "description", maxlength: 200, placeholder: "CT, room 3", autocomplete: "off" }),
  };
  const title = h("h2", {}, "Add a modality");
  const message = h("div");
  const save = h("button", { class: "primary", type: "submit" }, "Add");
  const cancel = h("button", { type: "button", class: "ghost", hidden: true, onclick: () => reset() }, "Cancel");

  function reset() {
    inputs.ae.disabled = false;
    for (const input of Object.values(inputs)) input.value = "";
    inputs.port.value = 104;
    title.textContent = "Add a modality";
    save.textContent = "Add";
    cancel.hidden = true;
  }

  const form = h("form", {
    class: "card-body",
    onsubmit: async (e) => {
      e.preventDefault();
      const ae = inputs.ae.value.trim();
      save.disabled = true;
      try {
        await send("PUT", modalityUrl(ae), {
          host: inputs.host.value.trim(),
          port: Number(inputs.port.value),
          description: inputs.description.value.trim(),
        });
        message.replaceChildren(notice("ok", `${ae} saved.`));
        reset();
        saved();
      } catch (error) {
        message.replaceChildren(notice("error", error.message));
      } finally {
        save.disabled = false;
      }
    },
  },
  h("div", { class: "search-grid" },
    h("label", { class: "field" }, "AE title", inputs.ae),
    h("label", { class: "field" }, "Host", inputs.host),
    h("label", { class: "field" }, "Port", inputs.port),
    h("label", { class: "field" }, "Description", inputs.description)),
  h("div", { class: "form-foot" },
    h("div", { class: "muted" }, "The modality must also know this server: its AE title, address and DIMSE port."),
    h("div", { class: "actions" }, cancel, save)),
  message);

  const el = h("section", { class: "card" }, h("div", { class: "card-head" }, title), form);
  return {
    el,
    edit(modality) {
      inputs.ae.value = modality.ae_title;
      inputs.ae.disabled = true;
      inputs.host.value = modality.host;
      inputs.port.value = modality.port;
      inputs.description.value = modality.description;
      title.textContent = `Edit ${modality.ae_title}`;
      save.textContent = "Save";
      cancel.hidden = false;
      message.replaceChildren();
      el.scrollIntoView({ behavior: "smooth", block: "nearest" });
      inputs.host.focus();
    },
  };
}

const MOVE_STATES = {
  running: ["Running", ""],
  succeeded: ["Done", "ok"],
  partial: ["Partly done", "warn"],
  cancelled: ["Cancelled", "warn"],
  failed: ["Failed", "bad"],
};

/** Local route of the study or series a transfer designates, when known. */
function transferRoute(transfer) {
  const study = transfer.keys.StudyInstanceUID;
  if (!study || study.includes("\\")) return null;
  const series = transfer.keys.SeriesInstanceUID;
  return series && !series.includes("\\") ? seriesRoute(study, series) : studyRoute(study);
}

const TRANSFERS = {
  moves: {
    title: "Retrievals",
    id: "retrievals",
    empty: "No retrieval since the server started.",
    columns: [
      { label: "From", value: (m) => m.modality, cls: "mono" },
      { label: "To", value: (m) => m.destination, cls: "mono" },
    ],
    // What a retrieval brought to this server.
    route: (m, own) => (own && m.destination === own && ["succeeded", "partial"].includes(m.state) ? transferRoute(m) : null),
  },
  sends: {
    title: "Sends",
    id: "sends",
    empty: "Nothing sent since the server started: send a study or series from its page.",
    columns: [{ label: "To", value: (m) => m.modality, cls: "mono" }],
    route: (m) => transferRoute(m),
  },
};

/**
 * The retrievals (C-MOVE, `kind` "moves") or sends (C-STORE, "sends") since the server started,
 * refreshed while some are running.
 */
function transfersCard(kind) {
  const spec = TRANSFERS[kind];
  const signal = navigation.signal;
  const count = h("span", { class: "count" });
  const content = h("div", {}, loading());
  let own = null;
  let timer = null;
  const table = dataTable([
    { label: "Started", value: (m) => m.started_at, render: (m) => new Date(m.started_at * 1000).toLocaleString() },
    ...spec.columns,
    { label: "Level", value: (m) => m.level, render: (m) => h("span", { class: "badge" }, m.level) },
    {
      label: "UID",
      value: (m) => Object.values(m.keys).join(" "),
      render: (m) => {
        const uid = m.keys.SOPInstanceUID || m.keys.SeriesInstanceUID || m.keys.StudyInstanceUID || m.keys.PatientID || "";
        return h("span", { class: "mono ellipsis", title: Object.entries(m.keys).map(([k, v]) => `${k}: ${v}`).join("\n") }, uid);
      },
    },
    {
      label: "State",
      value: (m) => m.state,
      render: (m) => h("span", { class: `state ${MOVE_STATES[m.state]?.[1] || ""}`, title: m.status ? `Status ${m.status}` : null },
        MOVE_STATES[m.state]?.[0] || m.state),
    },
    {
      label: "Progress",
      value: (m) => m.completed,
      render: (m) => {
        const total = m.completed + m.failed + m.warning + (m.remaining || 0);
        const done = m.completed + m.failed + m.warning;
        const percent = total ? Math.round((done / total) * 100) : m.state === "running" ? 0 : 100;
        return h("div", { class: "move-progress" },
          h("div", { class: "progress" }, h("div", { class: "bar", style: `width: ${percent}%` })),
          h("span", { class: "muted" }, `${integer.format(m.completed)}${total ? ` / ${integer.format(total)}` : ""}`,
            m.failed ? h("span", { class: "bad" }, ` · ${integer.format(m.failed)} failed`) : null,
            m.warning ? ` · ${integer.format(m.warning)} warnings` : null));
      },
    },
    {
      label: "Error",
      value: (m) => m.error || "",
      wrap: true,
      render: (m) => (m.failures && m.failures.length
        ? h("span", { title: m.failures.map((f) => `${f.sop_instance_uid}: ${f.reason}`).join("\n") }, m.error || "")
        : m.error || ""),
    },
    {
      label: "",
      value: () => "",
      sort: false,
      render: (m) => {
        if (m.state === "running") {
          return h("button", {
            class: "small",
            onclick: async (e) => {
              e.currentTarget.disabled = true;
              try {
                await send("POST", `${ADMIN}/${kind}/${m.id}/cancel`);
              } catch {
                // Over meanwhile: the next refresh shows it.
              }
              refresh();
            },
          }, "Cancel");
        }
        const route = spec.route(m, own);
        return route ? h("a", { class: "button small", href: route }, "Open") : null;
      },
    },
  ], { sort: { column: 0, direction: -1 }, empty: spec.empty });

  async function refresh() {
    clearTimeout(timer);
    if (signal.aborted) return;
    try {
      if (own == null) own = (await loadModalities()).ae_title;
      const transfers = (await (await get(`${ADMIN}/${kind}`, "application/json", signal)).json())[kind];
      table.set(transfers);
      count.textContent = transfers.length;
      content.replaceChildren(table.el);
      if (transfers.some((m) => m.state === "running")) timer = setTimeout(refresh, 1000);
      else if (kind === "moves" && transfers.length) refreshStatistics();
    } catch (error) {
      if (error.name !== "AbortError") content.replaceChildren(notice("error", error.message));
    }
  }
  signal.addEventListener("abort", () => clearTimeout(timer));
  refresh();

  return h("section", { class: "card", id: spec.id },
    h("div", { class: "card-head" },
      h("h2", {}, spec.title, count),
      h("button", { class: "small ghost", onclick: () => refresh() }, "Refresh")),
    content);
}

/** Modality the sends started from local pages go to, kept while browsing. */
let sendTarget = null;

/**
 * Controls sending what `keys` designate at `level` to a modality (C-STORE), with the area
 * where they report: empty while the modalities load, and when none is registered.
 */
function sendControl(level, keys) {
  const messages = h("div");
  const control = h("span", { class: "actions" });
  loadModalities().then((data) => {
    const titles = data.modalities.map((m) => m.ae_title);
    if (!titles.length) return;
    if (!titles.includes(sendTarget)) sendTarget = titles[0];
    const target = h("select", {
      "aria-label": "Send to",
      onchange: (e) => (sendTarget = e.target.value),
    }, data.modalities.map((m) => h("option", { value: m.ae_title, selected: m.ae_title === sendTarget },
      m.description ? `${m.ae_title} · ${m.description}` : m.ae_title)));
    const button = h("button", {
      class: "small",
      title: "Send these instances to the modality (C-STORE)",
      onclick: async () => {
        button.disabled = true;
        const to = sendTarget;
        try {
          const job = await send("POST", `${modalityUrl(to)}/store`, { level, keys });
          messages.replaceChildren(h("div", { class: "notice ok" },
            `Sending ${integer.format(job.remaining ?? 0)} instance${job.remaining === 1 ? "" : "s"} to ${to}: `,
            h("a", { href: "#/modalities" }, "follow it in Sends"), "."));
        } catch (error) {
          messages.replaceChildren(notice("error", error.message));
        } finally {
          button.disabled = false;
        }
      },
    }, "Send");
    control.replaceChildren(h("label", { class: "inline-field" }, "Send to", target), button);
  }, () => {
    // Without the administration API (or access to it), no sending.
  });
  return { control, messages };
}

/** Destination of the retrievals started from this page, kept while browsing. */
let retrieveDestination = null;

/**
 * The header of a modality's views: its address, a verification button and the choice of the
 * destination of retrievals. Resolves to `null` when the modality is unknown.
 */
async function remoteHeader(ae, report, extra = []) {
  const data = await loadModalities();
  const modality = data.modalities.find((m) => m.ae_title === ae);
  if (!modality) return null;
  const choices = [
    ...(data.listening ? [[data.ae_title, `This server (${data.ae_title})`]] : []),
    ...data.modalities.filter((m) => m.ae_title !== ae).map((m) => [m.ae_title, m.ae_title]),
  ];
  if (!choices.some(([value]) => value === retrieveDestination)) retrieveDestination = choices.length ? choices[0][0] : null;
  const destination = h("select", {
    "aria-label": "Retrieve to",
    disabled: !choices.length,
    onchange: (e) => (retrieveDestination = e.target.value),
  }, choices.map(([value, label]) => h("option", { value, selected: value === retrieveDestination }, label)));
  return h("section", { class: "card title-card" },
    h("h1", { class: "mono" }, ae),
    h("div", { class: "subtitle" }, [modality.description, `${modality.host}:${modality.port}`].filter(Boolean).join(" · ")),
    h("div", { class: "actions" },
      echoControl(ae, report),
      h("label", { class: "inline-field" }, "Retrieve to", destination),
      data.listening ? null : h("span", { class: "warn-text" }, "The DIMSE service of this server is disabled: it cannot receive instances."),
      extra));
}

/** A button asking modality `ae` to send what `keys` designate at `level`. */
function retrieveButton(ae, level, keys, report, label = "Retrieve") {
  const button = h("button", {
    class: "small",
    title: "Ask the modality to send these instances (C-MOVE)",
    onclick: async (e) => {
      e.stopPropagation();
      if (!retrieveDestination) {
        report(notice("error", "No destination: enable the DIMSE service of this server, or register another modality."));
        return;
      }
      button.disabled = true;
      try {
        await send("POST", `${modalityUrl(ae)}/move`, { level, keys, destination: retrieveDestination });
        button.textContent = "Started";
        report(h("div", { class: "notice ok" }, `Retrieval to ${retrieveDestination} started: `,
          h("a", { href: "#/modalities" }, "follow it in Retrievals"), "."));
      } catch (error) {
        button.disabled = false;
        report(notice("error", error.message));
      }
    },
  }, label);
  return button;
}

/** Local copies of these studies: their numbers of instances by Study Instance UID. */
async function localStudies(uids) {
  const found = new Map();
  try {
    for (const item of await uploadedStudies(uids)) {
      found.set(text(item, "StudyInstanceUID"), number(item, "NumberOfStudyRelatedInstances") ?? 0);
    }
  } catch {
    // A convenience: the results are shown without it.
  }
  return found;
}

const storedHere = (study, local) => (local.has(study)
  ? h("a", { class: "badge ok", href: studyRoute(study), title: "Open the copy stored on this server" },
    `stored · ${integer.format(local.get(study))}`)
  : null);

function remoteView(view, state, ae) {
  setCrumbs([["Modalities", "#/modalities"], [ae]]);
  const messages = h("div");
  const report = (node) => messages.replaceChildren(node);
  const { inputs, grid, chips } = criteriaForm(state, () => submit());

  function submit() {
    const params = new URLSearchParams();
    for (const [key, input] of Object.entries(inputs)) {
      if (input.value.trim()) params.set(key, input.value.trim());
    }
    params.set("search", "1");
    const target = `${modalityRoute(ae)}?${params}`;
    if (location.hash === target) route();
    else location.hash = target;
  }

  const form = h("form", {
    class: "card-body",
    onsubmit: (e) => {
      e.preventDefault();
      submit();
    },
  },
  grid,
  h("div", { class: "form-foot" },
    chips,
    h("div", { class: "actions" },
      h("button", {
        type: "button",
        class: "ghost",
        onclick: () => {
          for (const input of Object.values(inputs)) input.value = "";
        },
      }, "Clear"),
      h("button", { class: "primary", type: "submit" }, "Search"))));

  const header = h("div", {}, loading());
  const results = h("div");
  view.append(header, h("section", { class: "card" }, h("div", { class: "card-head" }, h("h2", {}, "Search the modality")), form), messages, results);

  remoteHeader(ae, report).then((card) => {
    header.replaceChildren(card || notice("error", "This modality is not registered (anymore)."));
  }, (error) => error.name !== "AbortError" && header.replaceChildren(notice("error", error.message)));

  if (!state.get("search")) {
    results.append(h("section", { class: "card" },
      h("div", { class: "empty" }, "Enter criteria and search: the modality is queried at the study level (C-FIND).")));
    return;
  }
  const count = h("span", { class: "count" });
  const content = h("div", {}, loading("Querying the modality…"));
  results.append(h("section", { class: "card" }, h("div", { class: "card-head" }, h("h2", {}, "Studies", count)), content));
  (async () => {
    try {
      const { matches, truncated } = await remoteFind(ae, "STUDY", searchParams(state, "studies"));
      const local = await localStudies(matches.map((m) => text(m, "StudyInstanceUID")).filter(Boolean));
      const table = dataTable([
        { label: "Patient", value: name, render: (r) => name(r) || h("span", { class: "muted" }, "(no name)") },
        columns.patientId,
        columns.birthDate,
        columns.studyDate,
        columns.studyDescription,
        columns.accession,
        columns.modalities,
        columns.studySeries,
        columns.studyInstances,
        { label: "Here", value: (r) => local.get(text(r, "StudyInstanceUID")) ?? -1, render: (r) => storedHere(text(r, "StudyInstanceUID"), local) },
        {
          label: "",
          value: () => "",
          sort: false,
          render: (r) => (text(r, "StudyInstanceUID")
            ? retrieveButton(ae, "STUDY", { StudyInstanceUID: text(r, "StudyInstanceUID") }, report)
            : null),
        },
      ], {
        href: (r) => remoteStudyRoute(ae, text(r, "StudyInstanceUID")),
        sort: byDateDesc(3),
        empty: "No study of the modality matches these criteria.",
      });
      table.set(matches.filter((m) => text(m, "StudyInstanceUID")));
      count.textContent = `${integer.format(matches.length)}${truncated ? "+" : ""}`;
      content.replaceChildren(
        truncated ? notice("warn", `Only the first ${integer.format(matches.length)} matches are shown: refine the search.`) : "",
        table.el);
    } catch (error) {
      if (error.name !== "AbortError") content.replaceChildren(notice("error", error.message));
    }
  })();
}

function remoteStudyView(view, state, ae, study) {
  setCrumbs([["Modalities", "#/modalities"], [ae, modalityRoute(ae)], ["Study"]]);
  const messages = h("div");
  const report = (node) => messages.replaceChildren(node);
  render(view, async () => {
    const [header, studies, series, local] = await Promise.all([
      remoteHeader(ae, report, [retrieveButton(ae, "STUDY", { StudyInstanceUID: study }, report, "Retrieve the study")]),
      remoteFind(ae, "STUDY", { StudyInstanceUID: study }),
      remoteFind(ae, "SERIES", { StudyInstanceUID: study }),
      localStudies([study]),
    ]);
    if (!header) throw new HttpError("This modality is not registered (anymore).");
    const item = studies.matches[0] || {};
    setCrumbs([["Modalities", "#/modalities"], [ae, modalityRoute(ae)], [crumb.study(item)[0]]]);
    const table = dataTable([
      columns.seriesNumber,
      columns.modality,
      { ...columns.seriesDescription, label: "Description" },
      { label: "Body part", value: (s) => text(s, "BodyPartExamined") },
      { label: "Date", value: (s) => dateTime(s, "SeriesDate", "SeriesTime") },
      columns.seriesInstances,
      {
        label: "",
        value: () => "",
        sort: false,
        render: (s) => retrieveButton(ae, "SERIES", { StudyInstanceUID: study, SeriesInstanceUID: text(s, "SeriesInstanceUID") }, report),
      },
    ], { sort: { column: 0, direction: 1 }, empty: "The modality returned no series for this study." });
    table.set(series.matches.filter((s) => text(s, "SeriesInstanceUID")));
    return [
      header,
      titleCard(text(item, "StudyDescription") || "(no description)", dateTime(item, "StudyDate", "StudyTime"), [
        ["Patient", name(item)],
        ["Patient ID", text(item, "PatientID")],
        ["Birth date", formatDate(text(item, "PatientBirthDate"))],
        ["Sex", text(item, "PatientSex")],
        ["Accession number", text(item, "AccessionNumber")],
        ["Study ID", text(item, "StudyID")],
        ["Referring physician", formatName(text(item, "ReferringPhysicianName"))],
        ["Modalities", badges(values(item, "ModalitiesInStudy"))],
        ["Instances", number(item, "NumberOfStudyRelatedInstances") != null ? integer.format(number(item, "NumberOfStudyRelatedInstances")) : ""],
        ["On this server", storedHere(study, local)],
        ["Study Instance UID", study, { mono: true }],
      ]),
      messages,
      h("section", { class: "card" },
        h("div", { class: "card-head" }, h("h2", {}, "Series", h("span", { class: "count" }, series.matches.length))),
        series.truncated ? notice("warn", "The modality has more series than shown.") : null,
        table.el),
    ];
  });
}

// ---------------------------------------------------------------------------------------------
// Router

const ROUTES = [
  [/^\/?$/, searchView],
  [/^\/patient$/, patientView],
  [/^\/upload$/, uploadView],
  [/^\/modalities$/, modalitiesView],
  [/^\/modalities\/([^/]+)$/, remoteView],
  [/^\/modalities\/([^/]+)\/studies\/([^/]+)$/, remoteStudyView],
  [/^\/studies\/([^/]+)$/, studyView],
  [/^\/studies\/([^/]+)\/series\/([^/]+)$/, seriesView],
  [/^\/studies\/([^/]+)\/series\/([^/]+)\/instances\/([^/]+)$/, instanceView],
];

let currentPath = null;

function route() {
  navigation.abort();
  navigation = new AbortController();
  const fragment = location.hash.replace(/^#/, "") || "/";
  const split = fragment.indexOf("?");
  const path = split < 0 ? fragment : fragment.slice(0, split);
  const state = new URLSearchParams(split < 0 ? "" : fragment.slice(split + 1));
  const view = document.getElementById("view");
  view.replaceChildren();
  if (path !== currentPath) window.scrollTo(0, 0);
  currentPath = path;
  const section = path === "/upload" ? "#/upload" : path.startsWith("/modalities") ? "#/modalities" : "#/";
  for (const link of document.querySelectorAll(".topnav a")) {
    link.toggleAttribute("aria-current", link.getAttribute("href") === section);
  }
  for (const [pattern, show] of ROUTES) {
    const match = pattern.exec(path);
    if (match) {
      show(view, state, ...match.slice(1).map(decodeURIComponent));
      return;
    }
  }
  setCrumbs([crumb.search(), ["Not found"]]);
  view.append(notice("error", "This page does not exist."));
}

// Light or dark theme: index.html applies it before the first paint. Until the viewer picks
// one with the button, it follows the system's.

const THEME_KEY = "fortitude-theme";

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  const button = document.getElementById("theme");
  const label = theme === "dark" ? "Switch to light mode" : "Switch to dark mode";
  button.setAttribute("aria-label", label);
  button.title = label;
}

function setupTheme() {
  applyTheme(document.documentElement.dataset.theme === "dark" ? "dark" : "light");
  document.getElementById("theme").addEventListener("click", () => {
    const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    try { localStorage.setItem(THEME_KEY, theme); } catch { /* not remembered */ }
    applyTheme(theme);
  });
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", (event) => {
    let chosen = null;
    try { chosen = localStorage.getItem(THEME_KEY); } catch { /* none */ }
    if (chosen !== "light" && chosen !== "dark") applyTheme(event.matches ? "dark" : "light");
  });
}

window.addEventListener("hashchange", route);
setupTheme();
route();
refreshStatistics();
