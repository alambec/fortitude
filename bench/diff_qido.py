"""Compare QIDO-RS results of two DICOMweb servers loaded with the same dataset.

    python3 bench/diff_qido.py http://localhost:8043/dicom-web http://localhost:8045/dicom-web \
        bench/datasets/mixed/manifest.jsonl

Runs the query families of the benchmark (and a few more) on both servers, and compares the
matched UIDs and the values of the attributes both servers return.
"""

import collections
import json
import sys
import urllib.parse
import urllib.request

A, B, manifest = sys.argv[1], sys.argv[2], sys.argv[3]
entries = [json.loads(line) for line in open(manifest)]


def get(base, path):
    request = urllib.request.Request(base + path, headers={"Accept": "application/dicom+json"})
    with urllib.request.urlopen(request) as response:
        body = response.read()
    return json.loads(body) if body else []


def norm(value):
    if isinstance(value, dict):
        if value.get("vr") in ("DS", "IS") and "Value" in value:
            return {"vr": value["vr"], "Value": [round(float(x), 6) for x in value["Value"]]}
        if value.get("vr") == "CS" and "Value" in value:
            return {"vr": "CS", "Value": sorted(x.strip() for x in value["Value"] if x)}
        return {k: norm(v) for k, v in value.items() if k != "vr"}
    if isinstance(value, list):
        return [norm(v) for v in value]
    return value


key_tag = {"studies": "0020000D", "series": "0020000E", "instances": "00080018"}
queries = []
for patient in sorted({e["patient_id"] for e in entries})[:20]:
    queries.append(("studies", f"PatientID={patient}"))
for prefix in sorted({e["patient_name"][:3] for e in entries}):
    queries.append(("studies", "PatientName=" + urllib.parse.quote(prefix + "*")))
for year in sorted({e["study_date"][:4] for e in entries})[:8]:
    for modality in sorted({e["modality"] for e in entries}):
        queries.append(("studies", f"StudyDate={year}0101-{year}1231&ModalitiesInStudy={modality}"))
for study in sorted({e["study_uid"] for e in entries})[:15]:
    queries.append((f"studies/{study}/series", ""))
for series in sorted({(e["study_uid"], e["series_uid"]) for e in entries})[:15]:
    queries.append((f"studies/{series[0]}/series/{series[1]}/instances", ""))
queries += [("series", "Modality=US"), ("instances", "Modality=MR"), ("studies", "")]

problems = collections.Counter()
examples = {}
for resource, query in queries:
    kind = resource.rsplit("/", 1)[-1]
    path = f"/{resource}?{query}" if query else f"/{resource}"
    a = {r[key_tag[kind]]["Value"][0]: r for r in get(A, path)}
    b = {r[key_tag[kind]]["Value"][0]: r for r in get(B, path)}
    if a.keys() != b.keys():
        problems[("result set", resource.split("/")[0])] += 1
        examples.setdefault(("result set", resource.split("/")[0]), (path, len(a), len(b)))
        continue
    for uid in a:
        for tag in (set(a[uid]) & set(b[uid])) - {"00080005", "00081190"}:
            if norm(a[uid][tag]) != norm(b[uid][tag]):
                problems[(tag, kind)] += 1
                examples.setdefault((tag, kind), (path, a[uid][tag], b[uid][tag]))
print(f"{len(queries)} queries compared")
for key, count in problems.most_common():
    print(count, key, json.dumps(examples[key])[:300])
